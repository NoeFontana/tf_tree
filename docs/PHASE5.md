# tf_tree — Phase 5 Implementation Specification: Offline, Observability, and the Adoption Wedge

> **Companions:** `docs/PROJECT.md` (decision log), `docs/PHASE1.md`–`PHASE4.md`.

**Deliverable:** the artifacts that make `tf_tree` useful to people who have adopted nothing (D28). They point a tool at a bag, or attach a read-only process to a running system, and get something `tf2` cannot give them. That is the wedge, and it produces the evidence gating Phase 7. A **frozen transform index** turns a multi-gigabyte bag into a memory-mapped file queryable from sixteen dataloader workers at once; an **observability layer** surfaces transform pathologies that are otherwise invisible.

---

## 0.0 Implementation status

**In progress.** The live status table, in the style of `PHASE2.md` §0.0.

| Area | Status |
|---|---|
| §1 `FORMAT_VERSION = 3`, Phase 6 **header fields** reserved | **Done, for the header only.** No region slot is reserved: a Phase 6 spline region is a *twelfth* region and needs another `FORMAT_VERSION` ([`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md); §1.2). Header 320 bytes with ≥ 64 reserved (asserted); `layout_hash` `0x3D10_4195`; `doctor --explain-version`. |
| §2 Frozen arena (`.tft`) | **Done.** `Tree::open_frozen`/`Tree::freeze_to` and `tf_tree freeze --from-live` are wired; §2.1's bit-for-bit claim is tested (`crates/tf_tree/tests/frozen.rs`); §2.4's read path is gated (§12 criterion 2). |
| §3 Bag ingestion | **Partly done — MCAP only.** `tf_tree_ingest` is consumed by `tf_tree.ingest_bag` ([`0046`](./decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md)). §3.1 (with spill), §3.3's MCAP source and every §3.2 row **except `--on-clock-reset=split`** are implemented (`cargo nextest run --workspace`); pass one detects no time domain; a `.db3` is diagnosed and refused; `freeze_from_arrays` is absent. Report schema `tf_tree.ingest/2`. Every §3 test reads a recording this crate wrote (`tf_tree_ingest::fixture`); no committed file is a real rosbag2 bag. Throughput is gated (`just gate5`, [`0050`](./decisions/0050-what-ten-times-real-time-divides.md)). |
| §4 Offline Python API | **Done, including all three §4.4 deltas**, and `tf_tree.ingest_bag(path)` (`Tree.source` carries path and BLAKE3 digest; `publisher()` drops it). No `freeze_bag`: `ingest_bag(p).freeze(out)` is the composition. Of §4.2's helpers only `span` is API. §4.4 item 2 refuses a tree inherited across `fork()`. Gated by `just py-test`, `just py-test-freethreaded`, `tests/python/test_ingest.py`. |
| §5 Diagnostic counters | **Done**, §5.6 included. §5.4's long-lived per-thread `Guard` requirement is **WITHDRAWN (2026-09-09)**; sharding is not justified (§5.7). |
| §6 Diagnostics catalogue `TFT001`–`TFT019` | **Partly done.** All nineteen ids exist; ids are appended, never renumbered. `--json` (schema `tf_tree.doctor/1`), `--exit-code` and `--suppress` are wired. **Seventeen detect**; `TFT002`/`TFT003` detect nothing in any configuration (§12 criterion 6). Conditional skips: `rg -n 'CheckOutcome::skipped' crates/tf_tree_cli/src/checks.rs`. |
| §7 `tf_tree top` | **Done, both halves.** Read-only, *refuses* `--rw`, four panes in plain ANSI; `--web` on `std::net::TcpListener` (127.0.0.1:8787; `/api/tick`, schema `tf_tree.top/1`). Gated by `src/web.rs` unit tests and `crates/tf_tree_cli/tests/web.rs`. **Not done:** keep-alive; key handling. |
| §8 Visualization | **Deliberately not built** — this is the finished state, not a gap. |
| §9 Benchmark artifact | **Partial.** `just bench-report` emits `report/{results.json,index.html}` with the §9.3 provenance header, every §9.2 row and all four "where we are worse" entries; every comparison row is `UNAVAILABLE` with its reason on this host, as §9.3 prescribes. **Not done:** §9.1's container image, public sample recording and `tf_tree bench compare` (a decision record), and §12 criterion 7. §9.1's measurement is `just dds-bench` ([`0015`](./decisions/0015-the-bridge-fills-a-shared-arena.md); figures in `docs/benchmarks/tf2.md`). |
| §10 Open-source readiness | **Partial.** Name recorded ([`0008`](./decisions/0008-the-name-tf-tree.md)); licences, `NOTICE`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, `SUPPORT.md` in place. MSRV **1.87** (`just msrv`). `bench-gate` is a regression gate (`just bench-check`). `release.yml` and `wheels.yml` publish crates, wheels (PEP 740), four Linux archives and the SBOM (`scripts/sbom.py`; that step has never executed). Signed tags: warns, and refuses when `REQUIRE_SIGNED_TAGS` is `true`. **Outstanding:** the mdBook site; a signing key; the first-five-minutes path ([`0052`](./decisions/0052-the-first-five-minutes-nobody-runs.md), `draft`). **Declined:** `license headers` ([`0051`](./decisions/0051-the-licence-travels-with-the-artifact-not-the-file.md)). |
| §11 Test plan, §12 Gate | **Partial.** §11's **No network** row is done (`just no-network`). §12 criteria 1, 2, 4, 5 are met and gated. **Not done:** criterion 7 (§9's gap); §11's per-check fixtures (§6's). |

### What this development environment can and cannot gate

- **ROS 2 is available in a container** (`tf_tree/tf2-bench:latest`). MCAP's Rust reader needs no ROS.
- **The `mcap` crate is taken with `default-features = false`**: its defaults vendor C, violating `PHASE2.md` §2's no-C-build-step rule. `tf_tree_ingest` decodes zstd/lz4 chunks with pure-Rust `ruzstd` and `lz4_flex` behind the default-on `compression` feature (`just ingest-check` builds without; `IngestError::CompressedChunk` then covers all compressed chunks). `IngestOptions::max_chunk_uncompressed_bytes` (64 MiB), `max_chunk_expansion_ratio` (1024) and the zstd window are enforced before allocation; fixtures are described in `crates/tf_tree_ingest/testdata/ATTRIBUTION.md`.
- **No GPU.** CPython 3.12.3 on the host; `uv` fetches 3.14/3.14t for `just py-setup`.
- **The benchmark host cannot fairly run §9's comparison** (4 physical cores with SMT, `perf_event_paranoid=4`), so §9.3 applies: omit a row that cannot be measured fairly and say why. `tft_16_workers_rss` is `Sensitivity::Memory` and measurable (`just gate4`); `tft_open_vs_bag_parse` stays `unavailable`, since no artifact times both halves over one recording.

---
## 0. Scope

### In scope

§1 `FORMAT_VERSION = 3`; §2 frozen arena (`.tft`); §3 MCAP ingestion; §4 offline Python API; §5 diagnostic counters; §6 diagnostics catalogue (`TFT001`–`TFT016`, plus `TFT017`–`TFT019` from §6's amendments, so the shipped catalogue is **19**); §7 `tf_tree top`; §8.3 stored-sample iteration; §9 benchmark artifact; §10 open-source readiness.

### Out of scope — NORMATIVE

| Excluded | Why |
|---|---|
| `tf2_ros::Buffer` shim, arena → `/tf` egress | Phase 7, gated on this phase's evidence |
| Splines | Phase 6 — §1 reserves their header fields |
| Covariance, CoW branches | **Cut** by [`0009`](./decisions/0009-descoping-phase-6.md) — not deferred |
| Inter-host replication | Phase 8 |
| Compression in `.tft` | Breaks `mmap`. |
| A web *framework* | §7. An embedded static page and one JSON endpoint. |
| **All visualization work**, including a viewer channel, plugin or SDK dependency | §8 |

---

## 1. `FORMAT_VERSION = 3` — break it once, deliberately

### 1.1 The honest finding
Phase 5 needs new arena regions, so this is a real break: every participant is rebuilt and restarted together, and no version-2 arena may be attached. The reservation is header bytes only; a Phase 6 region is a region and none is reserved, so a second break is owed ([`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md) keeps the ledger).

### 1.2 What goes in — NORMATIVE

| Region / field | Phase | Size |
|---|---|---|
| `EdgeCounters` array | 5 | `max_edges × 128 B` |
| `ParticipantCounters` array | 5 | `max_participants × 128 B` |
| Per-edge `nominal_rate_mhz: u32` | 5 | in `EdgeRecord` reserved bytes |
| Per-edge `declared_by_slot: u32` | 5 | in `EdgeRecord` reserved bytes |
| ~~`covariance_region_off: u32`, `covariance_stride: u32`~~ | — | **Descoped by [`0009`](./decisions/0009-descoping-phase-6.md).** The eight bytes stay reserved in place as `_reserved_covariance`; closing the gap would move the spline offsets below. |
| `spline_region_off: u32`, `spline_degree: u8` | **6** | header, zero when absent |
| `frame_kind: u8` (link / sensor / map / virtual) | 5 | `FrameRecord` reserved |
| Header `_reserved` | — | keep ≥ 64 bytes after all of the above |

Header fields whose Phase 6 content does not exist are declared with offset `0`, meaning absent. The **region table** reserves nothing: `crates/tf_tree_arena/src/layout.rs` declares `N_REGIONS` regions and none is a spline region, so a Phase 6 region is a *twelfth* and changes `ArenaLayout::total_size()`.

**NORMATIVE:** recompute `layout_hash` and bump `FORMAT_VERSION` to 3 in one commit. `tf_tree doctor --explain-version` prints both versions and the required action on a mismatch.

> **Amendment — the header is 320 bytes.** `ArenaHeader` was 256 bytes with 48 free (pinned by `header.rs`); the new fields plus ≥ 64 reserved need ≥ 77. `topo_lock` moves off offset 192. Reclaiming the descoped covariance bytes would move `spline_region_off` off 168, which `layout_hash` would not catch between two v3 participants. `layout_hash` does **not** cover region offsets, region count, or `max_frames`/`max_edges`/`max_participants`; its `strides` input is a `[u32; 12]` to which the two counter regions' strides are appended. The regions exist whether or not the `counters` feature is compiled in (§5.5, D34), so builds with and without it attach to each other. The `0x9075_90F5` literals in `tf_tree_ipc`'s wire tests are fixtures and do not move with the hash.

### 1.3 Publish-side counters need no storage at all

Push counters in `EdgeTelemetry` would cost ~5–10 ns per ~50 ns push to store what is already present: **push count** = `EdgeRecord::head`; **rate, jitter, gaps** = derivable from the contiguous stamp array; **last publish time** = the newest stamp. The push path is untouched. Only consumer-side failures need storage, and those increment on error paths: `EdgeCounters` (`crates/tf_tree_core/src/counters.rs`) holds `lookups_ok`, `err_extrap_before`, `err_extrap_after`, `err_no_data`, `err_slot_recycled`, `err_slot_contended`, `last_err_nanos` and `worst_extrap_gap_ns`, one 128-byte `#[repr(C, align(64))]` line per edge. `ParticipantCounters` mirrors it per participant slot, so `doctor` can say *which consumer* is failing.

---

## 2. The frozen arena — `.tft`

### 2.1 The file *is* the arena

Every internal reference is an offset, so the arena can be written to disk and mapped back with no parsing or fixups. A frozen `.tft` is a header, a manifest, and the arena bytes; opening one is an `mmap`.

**NORMATIVE:** the frozen read path uses the **identical** `Plan::at` code as the online path, against a `PROT_READ` mapping. No offline variant of the lookup, no separate index. The bit-identical replay test extends to `HeapArena` / `MappedArena` / `FrozenArena`.

### 2.2 Why this is the wedge

A perception team's dataloader re-parses the bag in every worker or precomputes poses into a pickle (losing arbitrary-time queries). With a `.tft`, sixteen workers each `mmap` the same file, the kernel shares one set of clean pages, and each worker queries at ~50 ns with no IPC (Phase 2 §3.8). §9 carries the row: total RSS across 16 workers versus 16 bag parses.

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

`arena_off` is **2 MiB aligned** so the mapping is eligible for transparent huge pages; a huge page needs address and file offset congruent modulo 2 MiB and the offset is the only half this format controls, so `MADV_HUGEPAGE` is best-effort and its failure is not an error. The manifest is CBOR because it is cold; everything hot is in the arena.

> **Amendment — the huge-page benefit is unverified.** `cargo run --release -p tf_tree_bench --features shm --example hugepage_grant` measures the grant; on the development host a resident arena is granted 0 KiB. The TLB-reach claim is a projection. A live arena is governed by `transparent_hugepage/shmem_enabled`, not `enabled` (§6, `TFT016`).

> **Amendment — the container header is 128 bytes.** The listed fields come to 120; `tf_tree_arena::frozen::FROZEN_HEADER_SIZE` pins 128 with 8 reserved, asserted by a test. The reserved tail is written zero and not checked on read, so a future field there must be optional by construction.

> **Amendment — the manifest's per-edge span and count.** `newest_ns` is exact; **`oldest_ns`** is the oldest sample *still retained in the ring*; both are `null` for an edge that has never published. The count is two keys: **`samples`** (`min(head, retained)`, what the file holds) and **`pushes_total`** (`EdgeRecord::head`, what the source produced). The per-edge map is eight keys.

### 2.4 Read path

The open is: `fstat` and `pread` of `FROZEN_HEADER_SIZE` bytes; validate `FrozenHeader` (magic, format_version, layout_hash, file_size); `check_extents` (arithmetic on the header's own offsets and lengths); `mmap(arena_off, arena_size, PROT_READ, MAP_PRIVATE | MAP_NORESERVE)`; best-effort `madvise(MADV_HUGEPAGE)`; validate the mapped `ArenaHeader`.

`validate_arena_header` (`crates/tf_tree_arena/src/check.rs`) touches exactly one page of the arena; §12 criterion 2's evicted arm uses that as its witness. There is **no checksum, no name-table walk, no manifest decode and no `source_digest` verification** on this path; every step is O(1) in the index size. A frozen arena has no socket, lock file or participant table, and `AttachMode` is permanently `ReadOnly`.

**NORMATIVE:** `layout_hash` mismatch is a hard error naming both values and stating that the file must be re-frozen. A `.tft` is a cache, not an archive; keep the source recording.

### 2.5 Sizing
Freezing computes exact per-edge capacities from a counting pass, so there is no wrap and no wasted space. A 30-minute recording with one 1 kHz edge, four 200 Hz edges and twenty static edges is ~233 MB. No special indexing is needed until a benchmark says otherwise.

---

## 3. Bag ingestion

### 3.1 Two passes — NORMATIVE

**Pass 1 (count).** Scan the recording; per edge, count samples and record `[t_min, t_max]`; collect frame names; detect edge kind. Output: an exact `ArenaLayout`.

**Pass 2 (fill).** Re-read, group by edge, **sort by stamp within each edge**, then push in order. Sorting is required because Phase 1 invariant 6 mandates non-decreasing stamps per edge and a recording is ordered by log time, not header time.

Memory is **capped** at `--max-memory` (default 4 GiB), spilling to a temporary run file with a k-way merge beyond that.

> **Amendment — the cap is enforced by two mechanisms.** (1) **Grouping:** edges are partitioned into sets whose buffers fit the cap together, and the recording is re-read once per set; preferred, and leaves no temporary file. (2) **The run file**, for one edge over the cap on its own: spill cap-sized sorted runs, then merge; a k-way merge holds one sample of every run resident, so runs are *reduced* in passes of bounded fan-in until one merge fits. Ties break by run index and runs merge in contiguous windows, so §3.2's "last occurrence wins" survives (`a_reduce_pass_keeps_the_last_occurrence`). `--max-memory` bounds the sort buffers and not the arena (`ingest::fill`; `tests/memory.rs`); the run index is outside the cap and reported as `FillStats::peak_run_index_bytes`.

> **Amendment — the stable sort's scratch is inside the cap.** "Last occurrence wins" needs a stable sort, which may allocate one extra copy of its buffer. A group's peak is `sum(buffers) + max(scratch)`; a spill run is sized against `2 × ENCODED`; `FillStats::peak_buffer_bytes` includes the term; §12 gate 5's cap (`grouped_cap_from`) carries the same reserve. Gated: `ingest::tests::groups_respect_the_cap` and `spill::tests::budget_fits_the_cap`. A non-allocating sort would move `spill::ENCODED` and is a decision record.

> **Amendment — pass one does not detect a time domain.** Every ingested edge takes the default `SystemDomain` (tag `0`; `ingest::fill` is the only `TreeBuilder` call site). A `TFMessage` and its MCAP records name no clock, and §3.3 deliberately does not use topic names for discovery; every other domain in the project is **declared**, never detected. Cost ([`0038`](./decisions/0038-the-domain-a-binding-cannot-name.md)): an arena of simulated stamps tagged as the system clock never raises `LookupError::TimeDomainMismatch`. Closing it is new public API on two crates and belongs in `docs/decisions/`.

### 3.2 Anomalies, all of which occur in real recordings

| Anomaly | Handling |
|---|---|
| Duplicate `(edge, stamp)` | Last wins (Phase 1 invariant 6). Count and report. |
| Stamps far in the future | Warn with the count and the worst offset; keep. |
| Zero stamps (`t == 0`) | Drop, count, report loudly. |
| Backward clock jump | Split into segments; `--on-clock-reset={split,halt}`. `split` produces multiple `.tft` files. |
| Static edge with differing values | Authority policy from Phase 4 §5.7; report both values. |
| Frame declared, never published | Kept in the tree, flagged by `doctor`. |
| Edge kind changes mid-recording | Hard error naming the timestamp. |

The ingest report is a first-class output: JSON alongside the `.tft`, summarized to the terminal.

> **Amendment — canonical order.** Frames and edges are declared in name-sorted order, not first-seen order, so shuffling a recording's messages (§11) yields identical `FrameId`s, `EdgeId`s and ring offsets.

> **Amendment — the clock-reset threshold and guard.** `tf_tree_bridge`'s `ClockGuard` threshold is reused so a recording and a live system draw the same line, but a backward stamp is **dropped online and kept offline**, because §3.1 sorts; jumps below the threshold are counted as `out_of_order` and kept. The guard is **per edge**: one guard over every edge halts on ordinary topologies (`map -> odom` is stamped at the scan and published later; two robots' clocks differ), which no threshold fixes. The error names the edge.

> **Amendment — `--on-clock-reset=split` stays refused.** (1) `--out` is a path, §2.3's container holds one arena, `open_file()` returns one `Tree` and `source_digest` identifies one recording, so `split` changes the output type everywhere. (2) There is no container for a segment set: segment stamps *overlap* and an arena has one time axis per edge. (3) The user almost never wants N of them; `halt` reports the edge, stamp and magnitude needed to cut the recording with `mcap filter` or `ros2 bag convert`. The parser accepts the variant and refuses with `IngestError::ClockResetSplitUnsupported`, so a typo is distinguishable from "not built".

### 3.3 Sources

- **MCAP** — primary. Read `tf2_msgs/msg/TFMessage` via the schema, not by assuming a topic name; support `/tf`, `/tf_static`, and remapped equivalents.
- **rosbag2 sqlite3** — convert to MCAP.
- **A running arena** — `tf_tree freeze --from-live --duration 60s`.
- **Python** — `tf_tree.freeze_from_arrays(...)` for poses not in a bag.

> **Amendment — the rosbag2 sqlite3 source is blocked on the dependency budget.** `rusqlite`/`libsqlite3-sys` vendors C, against `docs/PHASE2.md` §2; `prsqlite` records no licence, so `cargo deny check` refuses it; `sqlite-rs`/`sq3_parser` are header- and pager-level parsers only; writing one here is a b-tree walk in a crate whose reason to exist is not reimplementing databases. The remedy is `ros2 bag convert`. What is built is the diagnosis: `tf_tree_ingest::source::is_sqlite` checks the magic bytes and returns `IngestError::Rosbag2Sqlite`, and the CLI prints the conversion command (fixture: `testdata/rosbag2/`). It reopens for a permissively licensed pure-Rust SQLite reader.

---

## 4. Offline Python API

### 4.1 Identical to online — NORMATIVE

`ds = tf_tree.open_file("run.tft")` is an `mmap`; `ds.plan(a, b).at(stamps)` is the same call as online; `plan`, `at`, `at_into`, `adaptive`, `latest` — the same objects, semantics and bit-exact results. **Do not introduce a parallel offline API.**

### 4.2 Dataset helpers

Additions that only make sense when the whole timeline is present: `ds.span(a, b)`, `ds.edges()` (rate, jitter, gaps, count, span), `ds.gaps(a, b, threshold_ns=...)`, `ds.resample(a, b, t0, t1, hz=...)`, `ds.manifest`.

> **Amendment — one of these five shipped; the other four are decisions.** `span` is on `Tree` and works on a live tree too: `LatestCommon` generalized to a range. It returns `(t0, t1)`, with `t0 > t1` when the windows do not overlap (an empty intersection is an answer), and `None` when every step is static. An edge that has never published raises `NoDataError` naming its two frames. The arithmetic is `tf_tree_core::Plan::span`, which calls `check_generation`, so `TopologyChanged` and `ChildDetached` reach the caller (static branch: `crates/tf_tree/tests/behavior.rs`). `resample` is `plan.at(np.arange(t0, t1, 10**9 // hz))`; a second spelling is what §4.1 forbids. `edges()` and `gaps()` need §3's counting pass (a ring knows what it *retained*, not what the source produced), and `manifest` needs a CBOR *reader*, where the crate has only a writer.

### 4.4 Three API-contract deltas that land here — NORMATIVE

[`API.md`](./API.md) §6 rows 7, 8 and 9: each is a gap between Python and a surface that already has the feature.

**1. `Layout::QuatTwist` — derivatives reach Python and C (`API.md` §3.3).** A **fourth `Layout` variant**, not a fourth method: a contiguous `(N, 13)` write of `[qw qx qy qz tx ty tz | ωx ωy ωz vx vy vz]`. `LerpSlerp` returns `DerivativesUnavailable` here as it does from `at_with_derivatives`. On the C side it is one new `tft_layout` enumerator and a **minor** ABI bump (`PHASE4.md` §3.6).

> **Status: done, on all three surfaces.** `TFT_LAYOUT_QVEC7_WXYZ_TWIST6` is accepted by `tft_plan_at` and `tft_plan_at_many` and reachable from C++ as `layout_of<Quat7Twist6>`. `tf_tree_py` takes a keyword-only `layout=` on `at` and `at_into` (`"mat4"`, `"quat"`, `"affine32"`, `"quat_twist"`); `LerpSlerp` raises `DerivativesUnavailableError`. `build` and `open(create=...)` take `interp=`, defaulting to `"sclerp"` because `PROJECT.md` §5 D5 forbids a `LerpSlerp` default without a measurement. `PHASE3.md` §6.1's amendment is the single account of `NS_PER_STEP_ESTIMATE`.

**2. Introspection: `tree.frames()`, `tree.edges()`, `plan.edges()` (`API.md` §3.2).** `tree.edges()` is the *names* half only and differs from §4.2's `ds.edges()`; it must not acquire statistics, because a rate computed from a ring is the error §4.2 refuses.

**3. Exact stamp converters: `from_parts` / `from_timespec` / `from_ros` (`API.md` §5.1).** `from_sec` is lossy above 10⁷ seconds; the fix is an exact, total converter on every surface, none taking a float. `tf_tree.from_ros` converts a `builtin_interfaces/Time` exactly and **never** via `to_sec()`.

> **Status: done** (`API.md` §5.1's amendment). All three refuse rather than normalise or wrap; `tests/python/test_api.py`'s `PARTS_TABLE` and `crates/tf_tree_c/tests/abi.rs`'s `PARTS_TABLE` are the same ten rows asserted on each side. Python has no `from_timespec`; C has both.

### 4.3 The dataloader pattern
Document it, do not ship a class (a `torch.utils.data.Dataset` subclass would bind us to a framework version): open the `.tft` lazily, per worker, on first `__getitem__`.

> **Amendment — the reason for the lazy open.** Phase 3's fork poisoning does **not** apply to a `.tft`: `Tree::from_frozen` goes through `fork_gen_for`, which returns `None` for `ArenaBacking::Frozen`, so a child inherits the mapping intact (`tests/python/test_frozen.py`). The lazy open exists because **a `Tree` cannot be pickled** and a `DataLoader` with `num_workers > 0` pickles the dataset under `spawn` and `forkserver` (CPython 3.14's default on Linux). Opening per worker is also what §2.2's page-sharing depends on.

---

## 5. Diagnostic counters

### 5.1 "Telemetry" is the wrong word — NORMATIVE

Call them **counters**, or **diagnostic counters**. Never "telemetry", in the code, docs, CLI or changelog: some teams block on the word at procurement, and nothing here leaves the machine.

> **Amendment — this is an enforcement item.** `telemetr` has zero hits in code; the only occurrences are in this document and `PHASE4.md` §3.1's unstable-header table. `EdgeTelemetry` in §1.3 is a rejected hypothetical. The deliverable is a CI check that keeps the word out.

**The `tf_tree` *library* opens no network sockets. Ever.** The only socket in the library is the Phase 2 `AF_UNIX` rendezvous socket. Phase 8 replication will be a separately named, explicitly enabled component. **`tf_tree` is also the shipped binary, and it has exactly one exception: `tf_tree top --web`**, which binds an `AF_INET` listener only when an operator types the flag, loopback unless they name another address (§7's amendment).

**NORMATIVE CI test:** run the **library's** test suite under `strace` and assert that `socket(2)` is called only with `AF_UNIX`.

> **Amendment — the assertion exists.** `just no-network` (`scripts/no-network.sh`, which carries the PROVES / DOES NOT PROVE header) runs in `ci.yml`'s `shm` job on both matrix rows. It is scoped to the library's suite because `tf_tree top --web` binds `AF_INET` by construction.

### 5.2 The two kinds of counter have opposite cost profiles

**Error-path counters** (extrapolation, no-data, recycled, contended) cost **zero**, since the branch is already taken, and are irreplaceable: you look at them *after* something went wrong. The **success denominator** `lookups_ok` is an atomic increment on the hottest read path, on a per-edge line shared by every reader; it is a convenience that turns an error *count* into an error *rate*.

### 5.3 Error-path counters are always on, with no runtime switch — NORMATIVE
Diagnostics that are off by default do not exist when you need them. Error-path counters cost nothing, cannot affect a lookup result, and are the basis of `TFT010` and `TFT011`. No environment variable, no runtime flag.

### 5.4 The denominator batches in the `Guard`, so it is also free

A `Guard` is per-thread and spans a batch of lookups, so `lookups_ok` accumulates in a plain `Cell<u32>` on the `Guard` (`!Sync` by construction) and flushes once, relaxed, on `Drop`.

> **Amendment — the requirement for a long-lived per-thread `Guard` on the convenience path is WITHDRAWN; the convenience path keeps its per-call `Guard`.** It contradicts the batching argument: `Guard`'s `Drop` is the **only** thing that publishes `lookups_ok`, while `note_err` writes error counters straight through, so a guard that never ends holds the denominator and publishes the numerator, and `TFT010` would read 100% on a healthy edge. It is also unsound as specified: a `thread_local!` needs a `Guard<'static>`, a **second lifetime extension** (a decision record, [`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md)), and `Tree` is `Send + Sync`, so the handle can drop on another thread while a cached guard's destructor writes into freed memory. The price is `just guard-cost` ([`EVIDENCE.md`](./benchmarks/EVIDENCE.md)); it does not decide this. R2 governs the *hot* tier (`Plan::at` takes `&Guard`); a per-call `Guard` neither allocates, locks nor converts. The convenience path credits its denominator on **every call**: `the_convenience_path_publishes_its_denominator_on_every_call` (`crates/tf_tree/tests/counters.rs`).

### 5.5 The one switch that should exist is compile-time

For a certified or minimal build, the knob is the **default-on cargo feature** `counters` (`default = ["counters"]`), not a runtime flag. Disabling it removes the fields, the increments, and the regions' *use*; the regions remain (D34), so the layout hash does not fork.

> **A read-only participant keeps no counters.** D18 makes a consumer's attachment read-only, so *any* write from a read path faults, and the `Guard` flush is one. A read-only participant silently records nothing; `ArenaView` carries a `writable` flag, default `false`. So `TFT010` and `TFT011`, which are about consumers, see nothing on a publish-only publisher plus N read-only consumers; the disclosure is `no_counter_evidence`'s skip reason, which points at `tf_tree participants`. A writable counters region for read-only consumers is a decision record.

### 5.6 Counters are captured in snapshots

**NORMATIVE:** `tf_tree freeze --from-live` copies the counter regions into the `.tft`, and `doctor --json` output is timestamped and appendable, so a field snapshot carries the diagnosis.

> **Amendment — the counters are in the arena image, not the manifest.** `ArenaLayout::edge_counters()` and `participant_counters()` land at their own offsets and are read through `ArenaView::edge_counters`, so there is no second source of truth. The manifest keeps what the arena cannot hold.

### 5.7 What must still be measured
Confirm under sixteen concurrent readers that flush-on-drop shows no measurable contention; if it does, shard by participant slot.

> **Measured** by `cargo run --release -p tf_tree_bench --bin counter_cost`: the counters-on/off ratio is flat across 1–8 threads (about 2.6 ns per lookup), so there is no contention and **the sharding fallback is not justified**. §5.5's compile-time switch removes the cost for a build that cannot pay it. The control build needs `tf_tree_core` with `default-features = false` (verify with `cargo tree -e features`).

---

## 6. The diagnostics catalogue

`tf_tree doctor` gives each check a stable identifier so it can be suppressed, tested, and referenced.

| ID | Check | Severity | Detection |
|---|---|---|---|
| `TFT001` | Multi-publisher conflict on an edge | error | Phase 4 §5.4 counters, or two claim attempts |
| `TFT002` | Static transform republished with a different value | error | ingest / bridge comparison |
| `TFT003` | Edge kind changed (static ↔ dynamic) | error | edge record vs incoming |
| `TFT004` | Clock skew between publishers | warn | per-publisher `ClaimRecord::clock_offset_nanos` (see §6's amendment) |
| `TFT005` | Stamps in the future | warn | newest stamp vs now, per edge |
| `TFT006` | Zero or absurd stamps | error | value check during ingest and push |
| `TFT007` | Publish rate deviates from nominal | warn | derived from stamps vs `nominal_rate_mhz` |
| `TFT008` | Jitter: inter-arrival spread about the edge's own centre | warn | derived from stamps; **amended below**, this column read *"p99 inter-arrival ≫ nominal"* |
| `TFT009` | Gaps / dropouts, **including the one that has not ended** | warn | derived from stamps; the trailing gap `now - newest` needs a live arena and a wall-comparable clock, and discloses when it could not run |
| `TFT010` | Extrapolation hotspot | warn | `EdgeCounters` + participant attribution (skips on an arena that has served no lookups — see the amendment below) |
| `TFT011` | Ring capacity too small for observed consumer lag | warn | worst extrapolation gap vs buffer span, **or** `capacity × period` vs observed publish latency; skips only when neither has evidence |
| `TFT012` | Disconnected subtree | error | topology walk |
| `TFT013` | Frame declared but never published | info | head == 0 after a grace period |
| `TFT014` | Participant or claim slot leak | warn | Phase 2 lock file vs arena records |
| `TFT015` | Arena occupancy > 80% (frames, edges, participants) | warn | header counters for frames and edges; **participants is the lock file's, not the header's** (amended below) |
| `TFT016` | THP disabled, or `RLIMIT_MEMLOCK` below arena size | info | `/sys`, `/proc/self/limits` |
| `TFT017` | Dynamic edge with no live writer | warn | claim table (added by the amendment below) |
| `TFT018` | Stamps arriving out of monotonic order | error | observed push stream (added by the amendment below) |
| `TFT019` | A wall-clock domain stepped backwards — `TFT018`'s cause, not a publisher fault | warn | `TFT018`'s evidence + the edge's domain tag (added by the amendment below) |

`TFT016`'s evidence is text-parsed from `/proc/self/limits` by `crates/tf_tree_cli/src/hostfacts.rs`. Its finding does not predict that `mlockall` will fail, since that charges the whole address space; it says its silence is not a clearance and names `MCL_ONFAULT` ([`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md)).

Output modes: human (coloured, grouped by severity), `--json` (stable schema), and `--exit-code[=error|warn]` so `doctor` can gate a robot's startup or a CI job.

> **Amendment — `TFT015`'s participants row.** `ArenaHeader::participant_count` is never incremented and cannot be (a killed participant cannot decrement it, D17), and the arena participant table fails too, because a read-only attachment (D18) writes **no arena record**. Which lock-file population replaces them is [`0056`](./decisions/0056-the-participant-numerator-is-the-lock-files.md)'s (`draft`) open question. The row is absent and disclosed in `Meta.notes`.

> **Amendment — `--exit-code` has a `warn` tier.** On a live arena four of the six `Error` ids structurally skip (`TFT001`–`TFT003`, `TFT018`), while almost everything an operator is paged about is `Warn`. Bare `--exit-code` still means `error`; `warn` is *warn-and-above*. `Report::is_healthy` backs it.

> **Amendment ([`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md)) — `TFT004` reads the offset; it does not compute it.** `ClaimRecord::clock_offset_nanos` is the per-publisher offset, differenced by the writer (`0` means *no sample yet*; an exact zero is stored as `1`; only `SystemDomain` edges record anything). Four skips: `== 0`, `TFT005`'s epoch condition, a frozen `.tft`, and a **replayed** source. One sample cannot separate clock error from stamp-to-push latency, so `TFT004` fires only past `checks::OFFSET_BEYOND_ANY_PIPELINE_NS` (ten seconds): it finds a machine whose NTP never came up or whose RTC is dead, not PTP-scale drift, which needs a series (`crates/tf_tree_cli/src/top.rs`'s module header carries the owed rule). The fleet spread ships as a report note; the finding text names replay as the alternative reading.

> **Amendment — `TFT007` reads a declared rate.** It comes from the topology file (`rate_hz` -> `TopologyConfig::builder` -> `EdgeCfg::nominal_rate_hz` -> `TreeBuilder::build` -> `EdgeRecord::nominal_rate_mhz`; §1.2). **`0` means undeclared, not 0 Hz**: such an edge is not compared, and when *no* edge declares one the check skips and names the knob. The observed rate is the median inter-arrival; both directions fire; a partial run says so in `Meta.notes`, and a run that compared *nothing* skips. **A measured rate is not a declared rate**: `tf_tree topology --discover` writes a *measured* rate into `rate_hz`, so a recording of a publisher degraded at 12 Hz declares 12 Hz nominal. A discovered `rate_hz` is a starting point to review; `--discover` prints each edge's sample count to stderr. The reference fixture declares no rate, so `TFT007` is not run there.

> **Amendment — the catalogue runs to `TFT018`.** `unclaimed-dynamic` and `out-of-order`, the two Phase 1 checks with no id, are **appended, not folded in** (`TFT013`, `TFT014` and `TFT006` mean something else). Ids are a public contract (`--suppress`, `--json`, runbooks), never renumbered or recycled. Severity is preserved (`checks::tests::the_two_new_ids_keep_their_phase_1_severities`). `out-of-order` skips on a live arena because a ring being written while read can show the next lap's sample at the old end of the window. The `uncatalogued` array stays in the `--json` schema with no producer.

> **Amendment — `TFT019`: `CLOCK_REALTIME` is not monotone, and the failure reads like our bug.** ([`API.md`](./API.md) §5.3.) NTP steps and leap seconds surface as a **burst of `NonMonotonicStamp` rejections** (`PHASE1.md` §2 invariant 6) that `TFT018` reports as "a publisher restarted without resetting its clock". **`TFT019` is an attribution, not a second detector**: on `TFT018`'s evidence plus the edge's **declared domain tag** (`EdgeRecord::domain`), a run of at least eight consecutive rejections (`checks::CLOCK_STEP_MIN_REJECTED_RUN`) on a wall-clock edge is a clock step and the publisher is not at fault. It fires only on tag 0; on any other tag it **skips with a reason naming the tag** (`checks::tag_refusal`; `checks::tests::tft019_fires_only_on_the_wall_clock_tag_and_names_the_tag_it_refuses`). Sim time's harder question is settled by [`0012`](./decisions/0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md)'s `rcl` signal. It does not demote `TFT018`, and does not reuse the bridge's `dropped_non_monotonic` counter (`PHASE4.md` §5.5). Anything published at rate should declare a steady or PTP domain (`API.md` §2.5); `RUNBOOK.md`'s `NonMonotonicStamp` section carries both.

> **Amendment — `doctor` reads recordings, and the skip is keyed on the property that decides it.** `--from-bag <recording.mcap>` ingests through `tf_tree_ingest::run` and `--from-file <index.tft>` opens a frozen arena through `Tree::open_frozen`; both hand the catalogue an ordinary `Tree`. `TFT018`/`TFT019` run on `--from-bag` and **skip on `--from-file`**: `SampleRing::push` rejects an out-of-order stamp, so a ring (live, `.tft`, or §3.1's sorted arena) holds only accepted pushes. The skip is keyed on `tf_tree_cli::checks::PushStream` (`Observed` and `Recorded` run; `RingsAtRest` and `RingsUnderWriter` skip). `TFT001` is re-keyed by the same enum: a recording has no sender field, so **a bag cannot answer the multi-publisher question**.

> **Amendment — `TFT010` and `TFT011` skip on evidence, not source.** An arena built from a recording has never been read, so every `EdgeCounters` field is zero, which is also what a healthy heavily-exercised arena looks like. Both skip via `tf_tree_cli::checks::no_counter_evidence` (any lookup counts as evidence); `TFT011` skips only when *both* halves are blind, and the surviving half is disclosed in `Meta.notes`. **`TFT017` is deliberately not skipped**: a bag-built arena has no writer, and a fleet whose publishers all died reaches the identical state.

> **Amendment — `TFT014` detects the participant-slot leak; nothing is reclaimed.** The claim half was blind in the state the other half is about: it read `owner_pid == 0` from `ParticipantTable::identity`, which answers for any record whose `state` reads `LIVE`, which a participant killed without `Drop` leaves behind (`PHASE2.md` §5.1; [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 6). The owner's hangup reap releases a `SIGKILL`ed joiner's record, so a finding means a slot the reap cannot reach: the owner's own slot, an owner killed mid-reap, a client `epoll::add` failed for, a takeover heir's inherited peers, or a byte-less `TreeBuilder::build_shared` creator ([`0031`](./decisions/0031-the-participant-record-with-no-byte.md): out of contract, and `TFT014` **accuses** it; `a_byteless_record_in_a_served_arena_is_accused_of_leaking`). `Snapshot` carries one liveness answer per slot, taken once (`Tree::participant_alive`: `F_OFD_GETLK` on the slot's lock byte, else the `/proc` inference). No new id, no new arena field; severity warn; detection only, since a `doctor` check must not mutate a robot's arena. It **skips on `--from-file`** (a freeze copies participant records, so every slot names an exited process; `checks::SlotTable`).

> **Amendment — `doctor` opens the lock file (`0028` step 6).** On `--attach`, `checks::slot_leak` composes the lock-file byte, the identity record and the pid. A `RESERVED` record is reported when its byte is free. **The fork case is its own finding**: byte *held*, recorded pid gone (a forked child inherited the descriptors); the remedy is upstream (stop the child or fork+exec), and [`0030`](./decisions/0030-the-atfork-handler-and-inherited-descriptors.md) closes it at the source. It is judged from the lock file alone, so the *read-only* inheritor is reported too. Undetected: a claim whose slot has since been re-granted (an arena format change). The `/proc` half is three-valued (`recorded_given`: running / gone / cannot say): only `ENOENT` on a host that would have shown an entry proves death (`proc_answers_here`). A recorded pid is namespace-local ([`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md)), so `recorded_given` answers *cannot say* when the recorded PID namespace differs from the observer's (`0` is *unknown*). The finding names the lock-file identity's pid and states `byte free`, `byte still HELD`, or `byte not probed`. The word-before-byte order (`0028` piece 2) is pinned by signature: `Snapshot::probe_lock_facts` hands a callback the already-captured row.

> **Amendment — `TFT008` is the inter-arrival *spread*.** What shipped since Phase 1's `inconsistent-rate` is the **coefficient of variation** of the retained intervals about their own mean. Judging against the declared nominal is what `TFT009` already refuses for gaps, and a nominal exists only where a topology file declared one. A second statistic under the same id is forbidden by the `TFT017`/`TFT018` amendment. The rule is `tf_tree_cli::doctor::check_inconsistent_rates`; the argument is in `checks::tft008`'s doc.

> **Amendment — `TFT007` and `TFT008` withhold on stopped publishers.** Every rule in the catalogue measures **between** retained stamps, and a publisher that stopped three weeks ago leaves a full ring of perfectly spaced samples, so both would clear an edge `TFT009` calls dead. The pair prints only on a **live** source (`checks::live_wall_now`: `Clock::Wall` *and* `PushStream::RingsUnderWriter`), on an edge whose ring supports an `IntervalShape` (monotone, positive median, at least four intervals) and that has been silent for more than `GAP_FACTOR` x that median. They are **not given a finding** (a second warn id for one fault inflates `--exit-code warn`): each **withholds judgement** on such an edge, and where that leaves nothing judged the check **skips** naming the stopped publisher. `TFT008` also skips when no edge retained enough intervals (`doctor::SPREAD_MIN_INTERVALS`). **One predicate**, `checks::stopped_publishers`, serves all three checks and answers nothing on a recording or `.tft`; disclosure is `Meta.notes` via `checks::stopped_publisher_note`, and `checks::rate_coverage_note` reads the same map. `TFT017` is not in this set: a wedged publisher still holds its claim. Held by `checks::tests::a_stopped_publisher_is_not_certified_healthy_by_tft007_and_tft008`.

> **Amendment — `TFT009` skips over an empty subject set.** Both of its halves run only over edges `checks::interval_shape` accepted, so where every edge is declined the finding list is empty and rendered as `pass`. The floor is `GAP_MIN_INTERVALS` + 1 = five retained samples: transient for every arena's first four pushes per edge, permanent for an edge sized `RingSize::History { rate_hz, secs }` with `rate_hz * secs <= 4`. The skip reason is three-valued (`ShapeGap`: too few intervals, a **negative** interval — read `TFT018`, a non-positive median), one clause per class present (`checks::gap_evidence_skip`). No verdict moves; `pass` becomes `not run`. Tests: `checks::tests::tft009_skips_when_no_edge_retained_enough_intervals_to_measure_a_gap`, `checks::tests::an_out_of_order_stream_is_not_reported_as_a_dropout`.

> **Amendment — `TFT013` has the grace period its row requires.** The predicate was `kind == Dynamic && head == 0` with no time term, so `doctor` at bringup reported every dynamic edge. No declaration timestamp is added to the arena ([`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md)). The grace is how long the longest-running dynamic publisher has been running, `(head - 1) x median period`; `head` keeps growing across laps, unlike the retained span. *Longest* and *dynamic* are both load-bearing (`checks::tests::the_grace_period_reads_the_longest_running_dynamic_publisher`); the length is `checks::DECLARATION_GRACE_NS`. An arena in which nothing has published is a separate skip (bringup and total outage are indistinguishable; `TFT017` reports the second). **The grace evidence is *unobtainable* on some arenas, which is a third skip**: `doctor::median_period` needs two retained samples, and an edge with `rate_hz * secs <= 2` retains one for the life of the arena. `checks::publish_activity` returns `PublishActivity` (`NoPublisher`, `Unmeasurable { .. }`, `Running(ns)`), and the `Unmeasurable` reason branches between a ring that cannot hold two, a large ring given only one, and a publisher stamping one instant. Substituting a declared rate for a measured one is a §6 amendment. Tests: `crates/tf_tree_cli/tests/catalogue.rs::tft013_skips_with_the_ring_size_reason_on_an_arena_whose_publisher_it_cannot_measure`, `checks::tests::tft013_names_which_of_the_three_unmeasurable_arenas_this_is`.

---

## 7. `tf_tree top`

A live read-only participant. TUI first (topology with per-edge rate/staleness/occupancy and writer identity; participants; a rolling diagnostics feed; a per-edge detail view with an inter-arrival histogram), with an embedded static web view behind `--web`.

**NORMATIVE constraints on the web view:** a single embedded HTML file plus one JSON endpoint, no build step, no npm, no CDN. Charts in hand-written SVG. Bind to loopback by default.

> **Amendment — `ratatui` is not used.** `crates/tf_tree_cli/src/top.rs` draws four panes in about thirty lines of `ESC[H` / `ESC[K` / `ESC[J`, inside a workspace with a hard dependency budget. There is no key handling (raw mode means `termios`, `libc` and an `unsafe` boundary `tf_tree_cli` forbids), so the detail view is `--edge <id|name>`; there is no alternate screen. Interactive selection would be a decision record. Ages are against `checks::Clock::decide`'s reference clock (a majority vote of per-edge newest stamps against the host clock, falling back to the **median** newest stamp), which `top` labels with `Clock::label()`. "Observes without perturbing" is a test (`top::tests::capturing_the_arena_moves_no_counter`). Frame names and lock-file `comm` are sanitized by `top::sanitize`. The participant pane is the arena table ∪ the lock file (D18; §5.6). **Rates are observed and never presented as a deviation**: `top` shows a stamp-derived median rate and a head-advance rate side by side and compares neither against a declared one.

> **Amendment — the `--web` half.** **No HTTP crate**: `hyper`/`axum` pull a `tokio` runtime into a workspace with none. `--web` is `std::net::TcpListener` with a `std::thread::scope`d thread per connection, no keep-alive; it must never be pointed at a network (`serve`'s doc). **This is the only network socket in the repository**, and §11's "no network" test is scoped to the library's suite (§5.1). A **`Host` guard**: DNS rebinding makes any page the operator visits same-origin with `127.0.0.1:8787`, so a loopback bind refuses any request whose `Host` is not a loopback name, missing `Host` included; `--web 0.0.0.0:8787` gets a stderr warning and no guard. **"No CDN" is enforced by the browser**: every response carries `Content-Security-Policy: default-src 'none'` with `connect-src 'self'`, and two tests scan the page for protocol-relative `src`, `@import`, dynamic `import()` and the four string-to-DOM paths. A poll arriving inside one interval is answered from the previous document (one `Sampler` holds all per-tick state, so two tabs would each read half the true rate). **A peer that connects and says nothing is an outage**: handling is threaded, capped at `MAX_CONNECTIONS = 64`, with a read timeout retiring the socket (`silent_peers_do_not_delay_the_operators_poll`, `a_client_that_never_speaks_does_not_wedge_the_server`). The CSP test asserts against the response head only; `IntervalStats::rate_hz` and `observed_hz` return `None` unless positive.

---

## 8. Visualization — deliberately not built

### 8.1 The reasoning

A `tf_tree.rerun` module and a well-known-schema MCAP "viewer channel" solve a problem that does not exist: a user with a bag already has images, LiDAR, *and* transforms in one MCAP and opens it in Rerun or Foxglove. A transform channel `tf_tree` writes is a re-encoding of the same poses, costing a protobuf dependency, a schema to keep current, a CI job against two viewers, and a support surface. **`tf_tree`'s value is entirely in things a viewer cannot show**: how fast a query is answered, whether the answer is correct, and what is wrong with the transform tree.

### 8.2 What is genuinely not visible in a viewer — and where it goes

Clock skew (`TFT004`), extrapolation hotspots with consumer attribution (`TFT010`, `EdgeCounters`), multi-publisher conflicts (`TFT001`), rate deviation, jitter and gaps (`TFT007`–`TFT009`), buffer undersizing (`TFT011`) and live per-edge state (`tf_tree top`, §7) are not in the bag and are what `doctor` reports.

### 8.3 What survives — and it is not viewer-specific

For export, audit and statistics over stored data: `ds.iter_edge(edge, t0, t1)` (`(stamp_ns, pose)` at **stored** sample times, not interpolated), `ds.iter_edges(t0, t1)` (interleaved across edges, time-ordered) and `ds.frame_path("lidar")` (the root-to-leaf frame chain, already needed for error messages and `doctor` output).

**NORMATIVE:** `iter_edge` yields stored samples. Everything else in the API interpolates; "what was actually published" and "what would be interpolated at time t" are different questions. A Rerun snippet may ship as a documentation example, **never a module**.

### 8.4 If `tf_tree` ever becomes the source of truth
Transforms are genuinely missing from a recording only in a post-Phase-7 deployment where nodes publish to `tf_tree` instead of `/tf`. The **Phase 7 egress bridge** (arena → `/tf`) solves it with no viewer-specific code.

### 8.5 One idea explicitly parked
Annotating a copy of a recording with `tf_tree`'s *analysis* would require rewriting bags and has no requester. **Parked, unbuilt.**

## 9. The benchmark artifact

### 9.1 It is a product, not a script

`tf_tree bench compare --bag run.mcap --consumers 16 --duration 120s --out report/` runs both stacks on the same data: N `tf2` consumers versus one bridge plus N `tf_tree` consumers. It emits `report/index.html`, `report/results.json` (stable schema, CI-diffable), and the environment description needed to reproduce. Ship a container image and a small public sample recording.

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
| **Depth-3 hot lookup, `tf_tree` vs `tf2` (paired ratio)** | median per-round quotient, floored at 2.0× |

`report::tests::the_required_row_set_is_the_size_of_phase5_section_9_2s_table` counts this table's rows against `REQUIRED_ROWS.len()` in `crates/tf_tree_bench/src/report.rs`; it counts and does not match names.

The `Plan::at` row is `tf_tree` against itself: the only measurement of the path an **embedder** compiles. [`API.md`](./API.md) §2.3 makes the row and gate normative. Report it with the embedder's default profile, **not** this workspace's `lto = "thin"`, which hides the effect.

> **Amendment — there are two embedding measurements, and only the first is gated.** Both come from `just embed-cost` (`crates/tf_tree_bench/src/embed.rs`). (1) **The row above** — one build, one profile: two identical `#[inline(never)]` bodies, one in `tf_tree_bench`, one in `tf_tree_core` (`bench_probe`, a default-off feature), read off the `[profile.embedder]` run with `[profile.release]` as control; `embedding_cross_crate` in `results.json`, **gated at 5%** on `boundary_ratio`, `out_of_crate_ns` *and* `in_crate_ns` (a change moving both halves the same way passes a ratio-only gate), with verdict `unresolved` when the round-to-round band straddles 5%. (2) **Profile comparison** — the same out-of-crate body under `[profile.embedder]` against `[profile.release]`, two builds; **exploratory**, printed only. **CORRECTION (2026-09-06) — the criterion is currently unmeasured.** `Plan::at_tagged` sits between `Plan::at` and the fold with no `#[inline]`, so both columns compile to the same symbol: the quotient is 1.0 by construction and `Verdict::Over` is unreachable. `just embed-cost` refuses unless `EMBED_COST_KNOWN_COLLAPSED=1`; [`API.md`](./API.md) §2.3's 2026-09-06 amendment carries the run and the trade.

### 9.3 Honesty requirements — NORMATIVE

- Identical QoS, identical executor configuration, identical DDS vendor and version, all recorded in the report.
- Both stacks warmed; discard the first N seconds; state N.
- Report `tf2` version, ROS distro, RMW implementation, kernel, CPU model, and THP setting.
- **Report where `tf_tree` is worse**, in the same table and not in a footnote: arena memory floor, attach latency, the operational cost of a format bump, and the bridge as an additional process to supervise.
- Publish the harness source in the same repository. No private benchmark.

If a row cannot be measured fairly, omit it and say why.

**Amendment — "THP setting" is two knobs.** `transparent_hugepage/enabled` governs **anonymous** mappings and `shmem_enabled` governs **shmem**, which is what a live arena is; they disagree by default. The report records **both**, as `transparent_hugepage` and `transparent_hugepage_shmem`; `REQUIRED_FACTS` requires both, and a test reads each back against its file.

**Amendment — `Report::validate` enforces bullets 2, 3, 4 and half of 1 and 5.** Bullet 3's six facts and bullet 1's three are a closed list (`REQUIRED_FACTS`), present and non-empty; bullet 2's warm-up must be finite and non-negative, and **positive** whenever an `AbsoluteTiming` or `Ratio` row publishes numbers; bullet 5's mechanical half is that **every** row names a re-deriving command (`every_command_the_report_names_is_a_command_that_exists` resolves each against the real `justfile`).

**Amendment — an unavailable row's reason rests on a machine-checked `Ground`.** The decisive half of a reason is an enum `Ground` that `Report::validate` re-derives from `cfg!(feature = "tf2")`, `cfg!(all(feature = "shm", target_os = "linux"))`, the row's `Fitness` verdict, and the measured core count; `MeasuredElsewhere`, `NoInstrument` and `MeasurementRefused` are undecidable there. The three N-way rows (`cpu_per_consumer`, `publish_to_visible`, `scaling_curve`) lead with the permanent gap (this tool stands up no consumers) and a host-independent ground (`report::tests::a_host_with_no_obstacle_still_grounds_every_n_way_row`). `Ground` is not emitted into `results.json`.

**Amendment — "fairly" is three questions.** `Report::validate` asks each row the question its numbers rest on (`report::Sensitivity`):

An **absolute duration** (`AbsoluteTiming`) fails every check (debug build, SMT, busy machine, governor, unknown core count). An **interleaved ratio** (`Ratio`) fails on a debug build **and a busy machine**, and survives governor and SMT, which land on both arms of a within-round interleave. **Resident memory** (`Memory`) fails on a debug build or an unreadable `smaps_rollup`, and survives every timing check (Pss involves no clock). A **host-independent** figure (`HostIndependent`) fails nothing.

**Load is not common-mode between these two engines**: `tf2::BufferCore` takes a mutex on every lookup and `tf_tree`'s read path takes none, so a busy host **inflates the quotient in our favour**. The core budget does not reach a `Memory` row (sixteen workers mapping one `.tft` share the same pages), which makes **§12 gate 4 measurable on a 4-core host**. The `Ratio` axis's first row is `lookup_ratio_vs_tf2`: a depth-3 hot lookup on both engines in one process, arms interleaved within every round with the leading arm alternating, reporting the **median per-round quotient** ([`0025`](./decisions/0025-what-build-the-tf2-ratio-gate-speaks-for.md) records what build the gate speaks for). The tf2 column goes through `tf_tree_tf2_sys` and **flatters `tf_tree`** by the residual FFI boundary (`docs/benchmarks/tf2.md`, *Where the 45.3 ns comes from*). `ns_per_lookup` is reported and **never gated**.

**There are two committed baselines**, `results.json` and `results-tf2.json`, checked by `just bench-check` and `just tf2-bench-check`; a single baseline cut with `--features tf2` would fail the default recipe on every host without ROS 2. `baseline::compare` never reads a row's `reason` or `note`, so explanatory prose in a baseline can drift, and regenerating with `just bench-baseline-update` would launder a measurement change through a documentation fix. Owed: a check that baseline strings are still producible. The JSON keeps `timing_sensitive` with its original meaning, so `tf_tree.bench-report/2` does not change shape.

**Amendment — a one-sided BUDGET with a stated margin may be gated on a host that fails the timing probe (§12 criterion 2).** Criterion 2 is a **budget** (*under 10 ms*), and every timing check `Fitness::probe` fails makes a duration **longer**, so a **PASS with margin is conservative** and a **FAIL is not attributable to the code**. A gate taking this licence owes: (1) **the margin, per run, from the measurement** (criterion 2 prints its worst reading against the budget with the fitness verdict beside it); (2) **gating the arm that is a claim about the code** (the *resident* arm; the *evicted* arm's size dependence is the storage device, so it is reported); (3) a **debug build is not refused** (`frozen_open`) but prints its profile and says a debug FAIL is not attributable (`just gate2` builds `--release`). This is not a new `Sensitivity` variant and not a `bench_report` row: `tft_open_vs_bag_parse` stays `AbsoluteTiming` and `unavailable`, and the budget is held in its own binary and recipe. Applying this to a **two-sided** comparison, or to a budget whose margin is inside the host's noise, would be laundering.

---

## 10. Open-source readiness

Phase 5 is where the repository becomes publishable, so this is a deliverable.

- **Name check before anything else.** Confirm `tf_tree` is available on crates.io and PyPI, and decide whether the proximity to ROS's `tf` / `tf2` names helps or confuses.
- Apache-2.0 / MIT dual (D30), ~~license headers~~, `NOTICE`, SBOM per release. **The header clause is declined** ([`0051`](./decisions/0051-the-licence-travels-with-the-artifact-not-the-file.md)); the other three are done.
- `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md` with a real disclosure address.
- **A stated support policy**: what is supported, what is best-effort, and the response expectation.
- **MSRV policy** and a CI matrix pinning it.
- Documentation site (mdBook): a first-five-minutes path that works — `pip install transform_tree`, three lines, a real result — before any architecture prose. **Both halves are open and they are one question** ([`0052`](./decisions/0052-the-first-five-minutes-nobody-runs.md), `draft`): the three lines exist and run (`README.md`'s *Start with no data at all*, executed by `scripts/quickstart_smoke.py`), and nothing installs the published distribution and imports it.
- CI: the full Phase 1–5 suites on `x86_64` and `aarch64`, ASan/UBSan/TSan, Miri, loom, the nightly `shm_torture`, the benchmark artifact as a regression gate.
- Release automation: `cargo-dist` or equivalent, maturin wheels per Phase 3 §10, PEP 740 attestations, signed tags.

---

## 11. Test plan

- **Three-way bit-identity:** replay one recording into `HeapArena`, `MappedArena`, and `FrozenArena`; assert bit-identical `f64` (extends Phase 2 §10). `crates/tf_tree_cli/tests/replay_bit_identity.rs::a_replay_into_heap_mapped_and_frozen_arenas_is_bit_identical`, under `just shm-check`; the pairwise tests share no input and are kept.
- **Ingest anomalies:** a synthetic corpus containing every row of §3.2, asserting the whole ingest report JSON byte for byte: `crates/tf_tree_ingest/tests/anomaly_corpus.rs`. It is written by `tf_tree_ingest::fixture`, so it proves this reader's bookkeeping and not agreement with a real `rosbag2` or DDS writer.
- **Out-of-order ingest:** shuffle a recording's messages; the resulting `.tft` must be byte-identical to one built from the ordered source.
- **Spill path:** ingest with `--max-memory` below the dataset size; result identical to the in-memory path (`capped_memory_matches_the_uncapped_path`, `an_oversized_edge_spills_and_matches_the_in_memory_path`, `a_tiny_cap_reduces_in_several_passes`, `a_reduce_pass_keeps_the_last_occurrence`). Each asserts the reported peak from **both** sides.
- **Chunk decompression:** conformance against real libzstd (`a_real_libzstd_recording_ingests`, against `testdata/zstd_conformance.mcap`) is distinct from round-trip (`a_zstd_recording_ingests_identically`, `an_lz4_recording_ingests_identically`). Every bomb guard asserts the allocation, not only the error (`a_lying_uncompressed_size_is_refused_before_it_allocates`, `a_high_expansion_ratio_is_refused`, `a_zstd_frame_demanding_an_oversized_window_is_refused`, `the_window_floor_admits_what_a_real_zstd_encoder_declares`); length disagreements are `each_codec_round_trips_and_catches_both_length_disagreements`.
- **Ingest throughput:** time `tf_tree_ingest::run` against the recording's stamp span at the criterion's density, in two `--max-memory` regimes; gate the grouped one. `crates/tf_tree_bench/tests/ingest_throughput.rs` from `just test`; `just gate5` is the gate ([`0050`](./decisions/0050-what-ten-times-real-time-divides.md)).
- **Multi-process page sharing:** 16 processes mapping one `.tft`; total RSS within 1.2× of a single process, from `/proc/*/smaps_rollup` `Pss`.
- **Open time:** with the page cache **resident**, the open of a gate-scale index must fit 10 ms and agree with the open of a fixture two orders of magnitude smaller; with it **evicted** the number is reported and not gated. Each open is a fresh process and the verdict takes the worst. The evicted arm refuses unless the child's major-fault count witnesses the eviction; a gated run refuses a fixture under 233 MB, two fixtures too close in size, and a `--budget-ms` above the criterion's. `crates/tf_tree_bench/tests/gate2.rs` from `just shm-check`; `just gate2` is the gate.
- **Fork safety:** a `DataLoader` with `num_workers=16` under all three start methods.
- **Counter contention:** 16 concurrent readers on one edge; no measurable throughput difference against a `counters`-disabled build (§5.7).
- **No network:** the library's test suite under `strace`, asserting `socket(2)` is only ever `AF_UNIX` (§5.1), scoped to the library's suite (§7): `just no-network`. Its **positive control** is a separate trace over `crates/tf_tree_cli/tests/web.rs` that must find `AF_INET`. It refuses on a missing `strace`, a `strace` that cannot see a socket it is shown, a traced binary that exits non-zero, and a run in which `tests/rendezvous.rs` was not traced or opened no socket.
- **Convenience-path denominator:** `tree.lookup` in a loop credits `lookups_ok` **once per call** (§5.4): `the_convenience_path_publishes_its_denominator_on_every_call` (`crates/tf_tree/tests/counters.rs`).
- **Diagnostics:** one test per check ID, each with a fixture that triggers exactly that check and no other. **Not met**; §12 criterion 6 names the two ids that cannot meet it. `TFT005` pins `FUTURE_TOLERANCE_NS` at both edges; `TFT016` has a table over synthetic `HostFacts` (`checks::tests::every_tft016_arm_fires_and_the_two_corrected_strings_are_pinned`). `rg -n 'fn tft0' crates/tf_tree_cli/src/checks.rs` is the instrument for the rest.
- **`doctor --json`:** schema-validated; adding a check must not break an existing consumer. `catalogue::the_json_report_parses_and_matches_its_documented_schema` runs the real binary and holds the document to the schema block in `catalogue.rs` (top level only): the key set in both directions, the `tf_tree.doctor/1` identifier, every catalogue id once and in id order, and `reason` a string exactly when `status` is `"skipped"`.
- **Web view:** loopback binding asserted; no outbound network requests (assert on the served HTML); the `Host` guard; and an end-to-end test that parses the document a browser receives.
- **`iter_edge` returns stored samples:** push a known irregular sequence, iterate, and assert the exact stamps come back — no resampling, interpolation or reordering.

---

## 12. Gate

1. **Three-way bit-identity passes. MET since 2026-09-08** — `crates/tf_tree_cli/tests/replay_bit_identity.rs::a_replay_into_heap_mapped_and_frozen_arenas_is_bit_identical`, run by `just shm-check` and `ci.yml`'s `shm` job.
2. `.tft` open time under **10 ms** for a 233 MB index (an `mmap` plus header validation; anything more means work is happening that should not). **MET, and gated since 2026-09-05 — `just gate2`.** Every step of `Tree::open_frozen` is O(1) in the index size (§2.4), so the gate is a **regression guard rather than a discovery**. `just gate2` prints the run's own numbers (`--release`, worst of 8 fresh processes): the **resident-cache budget** (more than two orders of magnitude under 10 ms at 338 MiB) and **scale invariance** (a small multiple of the 2.1 MiB fixture's open, inside the 4× bound) are **GATED**; the evicted-cache open is reported.

   **Only the resident arm gates.** An open takes exactly one major fault when the cache has been dropped and zero when not, at both sizes; how much the kernel reads to satisfy it is a property of the file and mapping, so the evicted arm measures the storage stack. The words are `evicted`/`resident` because `crates/tf_tree_bench/src/bin/attach_bench.rs` uses *cold* for the first attach in a process. See [§9.3](#93-honesty-requirements--normative)'s one-sided-budget amendment. **The falsifier is `--prefault`**, which reads every byte of the index inside the timed region and puts the open tens of milliseconds over budget. A run it cannot evaluate **refuses**: a fixture under 233 MB; two fixtures too close in size; a **gated** run whose eviction did not take (`$TMPDIR` on tmpfs is the usual cause); a gated run against a `--budget-ms` **above** the criterion's. `crates/tf_tree_bench/tests/gate2.rs` drives all of it through the shipped binary. **Not covered:** a `.tft` is deliberately not prefaulted, so a gate on the open alone cannot see work *moved* into the first lookup.
3. Frozen lookup p50 within **20%** of online. **SUPERSEDED by §2.1, and answered by construction — nothing measures this, and nothing is owed.** `Tree::open_frozen` hands a `FrozenArena` to the same `&dyn Arena` the heap and `memfd` backings use (`ArenaBacking` in `crates/tf_tree/src/tree.rs`), which `a_frozen_lookup_is_bit_identical_to_the_live_one` (`crates/tf_tree/tests/frozen.rs`) asserts. There is no second implementation to take a ratio between; what differs is residency, which is gate 4's subject.
4. **16 workers sharing one `.tft`: total Pss within 1.2× of one worker.** **MET — 1.024×, and gated since 2026-09-04 by `just gate4`** (`crates/tf_tree_bench/src/bin/frozen_workers.rs`): 16 workers cost 235.5 MiB against 229.9 MiB for one on a 338 MiB frozen fleet arena; solving `total(N) = S + N·p` gives **S = 229.5 MiB shared, p = 0.37 MiB private per worker**. `just gate4` passes `--gate` and fails the process on a FAIL; `just gate4-python` does not and exits 0. `--gate` refuses `--python` and `--no-touch`, and refuses when there is no N = 1 or N = 16 row. `crates/tf_tree_bench/tests/gate4.rs` drives the shipped binary on a 2-robot fixture that misses `S ≥ 74p`; `nightly.yml`'s `gate4` job runs the gate and `just shm-check` runs the test. Until 2026-09-04 it was measured and not gated: `frozen_workers.rs` returned `Ok(())` on both PASS and FAIL ([`0023`](./decisions/0023-the-gate-that-could-not-gate.md)).

   Three things about the measurement: **the `.tft` has to be large** (`(S + 16p)/(S + p) ≤ 1.2` rearranges to `S ≥ 74p`, so the criterion is about *sharing* only once the arena is hundreds of MiB); **workers must actually read the file** (`--no-touch` measures an unread mapping: 5.32×, FAIL); **Pss must be sampled while every worker is alive**, behind a barrier, because Pss divides a shared page by the processes *currently* mapping it.

   > **Amendment — 1.024× is a statement about a *Rust* worker, and must be cited with the worker's language and start method attached.** `p` is a property of the worker. `just gate4-python` (`crates/tf_tree_bench/python/gate4_worker.py`, under the same `frozen_workers --python` driver) gives **1.804–1.806×** on CPython 3.14.3 (`p = 13.84–13.86 MiB`), so `S ≥ 74p` wants ~1 025 MiB where the fixture supplies 338. On a 39 MiB fixture the start methods separate:
   >
   > Measured `p` and the minimum `S` for `S ≥ 74p`: Rust (`frozen_workers.rs`, the gate's own) **0.36 MiB**, 27 MiB; forked CPython + numpy **3.36, 3.37 MiB**, 249 MiB; spawned CPython + numpy **13.4–14.2 MiB**, ~1 000 MiB.
   >
   > [`0026`](./decisions/0026-the-corpus-shape-of-a-frozen-index.md) reached the same conclusion from a different corpus. The wedge's audience is the spawned row (§4.3), and a torch worker imports far more than numpy, so 13.44 MiB is a floor; no torch `DataLoader` was in the loop, and raw `os.fork` is `0026`'s open question 2. **This amendment does not change criterion 4 or its MET.** Whether the gate acquires a second worker arm is a decision record; the binary refuses `--gate --python` with a message naming this paragraph. **Wherever 1.024× is cited as evidence for the wedge** (`README.md`, the §9 report, a talk) **it is cited as *a Rust worker sharing a 338 MiB `.tft`***; both recipes name their worker in the verdict line.
5. Ingest throughput ≥ **10× real time** on a representative recording. **MET, and gated since 2026-09-05 — `just gate5`.** [`0050`](./decisions/0050-what-ten-times-real-time-divides.md) answers what the ratio divides, what the density floor is for, why this may be gated on a host that fails the timing probe, and at what pass count the criterion is stated; read it before changing any of the four. `just gate5` prints the run's own numbers (`--release`, worst of 3 rounds, 50 edges × 100 Hz × 32 s zstd corpus): the **grouped** arm (pass two takes the group count this criterion's own recording forces) is **GATED**; the in-memory arm (default `--max-memory`, one fill pass) is reported. The falsifier is a **denser corpus**; `crates/tf_tree_bench/tests/ingest_throughput.rs` drives it per-PR in `just test`. `PHASE4.md` §6.3 carries a *different* "10× real time" criterion (ROS 2 bag replay); it is unmet, about the bridge, and unrelated.
6. Every §6 check has a passing fixture test. **Not met, and it cannot be met by writing tests.** `TFT002` and `TFT003` do not detect in any configuration `doctor` builds (`tf_tree_bridge::StaticStore` is process-local); the criterion stays unmet rather than being re-read as "every check that *can* detect". **`TFT002`**'s evidence is in `doctor`'s own process on the recording source (`Anomalies::static_conflicts`, printed to stderr and dropped); it needs a route into `checks::Inputs` **and a decision**: the count is a bare `u64` with no edge attached and counts *observations*, and as an arena-level `error` it would change the exit status of every `--from-bag` CI invocation. **`TFT003`** becomes `IngestError::EdgeKindChanged` on a recording, aborting the run, so *"`TFT003` fired"* and *"the recording ingested"* are mutually exclusive; making it fire means demoting a hard error to a counted anomaly in `tf_tree_ingest` (against §3.2 and §5.7), and doing it inside `doctor` would be a second ingest policy. Both are decision records.
7. Benchmark artifact runs from the published container on a clean machine and reproduces the committed `results.json` within tolerance.
8. §10 checklist complete, including the name decision.

Criterion 4 is the wedge's central claim, and criterion 7 is what makes it believable to anyone outside the team.

---

## 13. Definition of done

- [x] `FORMAT_VERSION = 3` shipped in a single commit, with Phase 6 **header fields** reserved and `doctor --explain-version` — `crates/tf_tree_arena/src/header.rs` carries `FORMAT_VERSION: u32 = 3` with the header at 320 bytes, and `layout::tests::layout_hash_is_deterministic_and_stable` asserts `layout_hash()` against a literal. The region half is retracted (§1.2, [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md)); §0.0's §1 row is authoritative. `explain_format_version` (`crates/tf_tree_cli/src/lib.rs`) is exercised by no test.
- [ ] Publish-side observability derived, not counted — push path unchanged and benchmarked to prove it — **the first conjunct is met and the second is not.** [`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md) put a clock-offset sampler on `EdgeWriter::push`, priced by `just push-sampler-cost` (registered in [`EVIDENCE.md`](./benchmarks/EVIDENCE.md), not a gate). Closing this box means amending its wording against `0036`.
- [x] Nothing in the codebase, CLI, or docs is called "telemetry" — `grep -rIl -i telemetry` (excluding `target/`, `.venv*/`, `.git/`) returns this document alone. Nothing enforces it (§5.1's amendment).
- [x] `socket(2)` restricted to `AF_UNIX`, asserted in CI — `just no-network`, in `ci.yml`'s `shm` job. Scoped to the library's suite per §11: the five published crates are traced, `tf_tree_cli` is the control, and every other package is traced by nothing.
- [x] Error-path counters always on with no runtime switch; `counters` cargo feature is the only knob — `crates/tf_tree_core/Cargo.toml` and `crates/tf_tree/Cargo.toml` both have `default = ["counters"]`; the regions stay either way (D34). `grep -rn 'var("TF_TREE' crates/tf_tree_core/src crates/tf_tree/src` finds none (§5.3). The counters-*off* build is compiled by `just lint`'s `cargo clippy -p tf_tree_core --no-default-features --features crash-points` and `just stable-tier-check`'s `cargo clippy -p tf_tree --lib --no-default-features`.
- [x] Convenience path keeps its **per-call** `Guard` — **§5.4's NORMATIVE requirement is WITHDRAWN (2026-09-09); read its amendment before reopening this.** Closed by the withdrawal plus `the_convenience_path_publishes_its_denominator_on_every_call`. `Tree::lookup_tagged` builds a `Guard` per call (`crates/tf_tree/src/tree.rs`); the per-thread structure is the plan cache (`crates/tf_tree/src/cache.rs`). [`0022`](./decisions/0022-the-per-call-guard-and-the-unwatched-gate.md) leaves the per-call guard deliberately, answering the cost with `tft_plan_at_many`.
- [x] `freeze --from-live` captures counters into the manifest — **met, and the wording is superseded by §5.6's amendment: they land in the arena image, not the manifest.** `freezing_carries_the_counter_regions` (`crates/tf_tree/tests/frozen.rs`), run by `just shm-check`.
- [x] `.tft` format implemented, 2 MiB-aligned arena, CBOR manifest, `source_digest` — `crates/tf_tree_arena/src/frozen.rs` carries `ARENA_FILE_ALIGN`, the manifest offset/length pair and `source_digest`; `crates/tf_tree/src/cbor.rs` is the manifest's **writer** (a definite-length RFC 8949 subset) and there is deliberately **no decoder**. Both are `#[cfg(all(feature = "shm", target_os = "linux"))]`, run by `just shm-check`.
- [ ] Three-way bit-identity test green in CI — the property was composed from three pairwise tests, one of which (`a_frozen_bag_answers_like_the_tree_it_came_from`, `crates/tf_tree_ingest/tests/frozen_bag.rs`) compares `Result<Iso3, LookupError>` by value and not `to_bits`. §12 criterion 1's single test now drives one recording into all three backings under `just shm-check`.
- [x] Offline Python API is the *same* API; no parallel surface introduced — `tf_tree.open_file()` and `tf_tree.ingest_bag(path)` ([`0046`](./decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md)) return the ordinary `Tree` (`python/tf_tree/_core.pyi`); no `freeze_bag`. Gated by `just py-test` and `just py-test-freethreaded`, two steps of `ci.yml`'s `python bindings (pytest, 3.14 + 3.14t)` job.
- [x] Ingest report emitted as JSON and human summary; every §3.2 anomaly covered by a fixture — `crates/tf_tree_ingest/src/report.rs` carries `to_json` (schema-tagged) and `summary`. Each §3.2 row has a test in `crates/tf_tree_ingest/tests/ingest.rs`: `duplicates_resolve_last_wins`, `zero_stamps_are_dropped_and_counted`, `future_stamps_are_kept_and_reported`, `edge_kind_change_is_a_hard_error`, `clock_reset_halts_but_jitter_does_not`, `static_conflicts_are_reported_and_first_wins`, `an_edge_whose_every_sample_was_dropped_is_declared_and_flagged`; the whole-document test is §11's `anomaly_corpus.rs`.
- [x] All 16 diagnostic checks implemented with stable IDs, `--json`, and `--exit-code` — the number is stale downward by design: `TFT001`–`TFT019` ship, and §0.0's §6 row is authoritative on how many *detect*. Tests: `catalogue::tests::identifiers_are_unique_and_round_trip`, `every_id_is_reported_and_every_skip_states_a_reason`, `the_json_summary_agrees_with_the_exit_status`, `the_exit_code_gate_has_a_warn_tier_and_an_unchanged_default`.
- [x] `tf_tree top` TUI plus embedded web view with no build step, loopback-bound
- [ ] `iter_edge` / `iter_edges` / `frame_path` present on both live and frozen arenas, with `iter_edge` yielding stored samples — **none of the three exists in any language.** Blocked on a record: [`0026`](./decisions/0026-the-corpus-shape-of-a-frozen-index.md) step 3 (per-episode versus per-corpus); its steps 6 and 7 landed as [`0027`](./decisions/0027-the-48-byte-frame-name-store.md), and its step 2, `freeze_from_arrays`, is the other half §0.0's §3 row records as absent. Before writing a signature: `API.md` §1 R1 and §7, and `CLAUDE.md`'s rule against a second spelling.
- [x] No viewer dependency, channel, schema, or plugin anywhere in the repository — §8's finished state. `grep -rn 'foxglove\|rerun\|rviz\|plotjuggler' --include=Cargo.toml .` returns nothing; nothing enforces it.
- [ ] Benchmark artifact reproducible from a published container by someone outside the team — **the artifact exists and the container does not.** `just bench-report` / `bench-report-shm` emit `report/{results.json,index.html}`, `crates/tf_tree_bench/baseline/results.json` is committed, and `just bench-check` gates it in `ci.yml`'s `bench-gate` job. `docker/` holds only `tf2`, published nowhere. A container must choose its build: `Tree::open_frozen` is `shm`-gated and the two `.tft` rows are `UNAVAILABLE` without it; §12 criteria 2 and 4 need a `--release --features shm` build, while `just gate5` needs no `shm`.
- [x] "Where we are worse" section present in the benchmark report — `crates/tf_tree_bench/src/report.rs` carries §9.3's topics verbatim; tests `report::tests::the_where_we_are_worse_entries_are_required_and_must_state_the_cost` and `report::tests::a_worse_entry_with_no_numbers_must_say_why`.
- [ ] §10 open-source checklist complete, name decision made and recorded — **the name decision is closed and the checklist is not.** [`0008`](./decisions/0008-the-name-tf-tree.md) records it (distribution `transform_tree`, module `tf_tree`); `license headers` is **declined** ([`0051`](./decisions/0051-the-licence-travels-with-the-artifact-not-the-file.md)). Open: the first-five-minutes path ([`0052`](./decisions/0052-the-first-five-minutes-nobody-runs.md)); the **mdBook site** (no `book.toml`, `SUMMARY.md`, recipe or workflow); and a **signing key** (every tag through `v0.0.5` is unsigned). `CONTRIBUTING.md`'s *Releasing* section carries the one-time setup.
- [x] §12 gate met, or a written explanation of which criterion failed and by how much — **the tick records that the explanation exists, not that the gate is met.** Criteria 1, 2, 4 and 5 are met and gated; 3 is superseded; 6 is partly met (`crates/tf_tree_cli/tests/catalogue.rs` proves a correct populated live tree stays quiet; the unit tests in `crates/tf_tree_cli/src/checks.rs` prove each check fires); 7 is **held by nobody** (box 16 above; the clean-machine half runs in `bench-gate`); 8 is partly met (box 18 above).
- [ ] `docs/PHASE6.md` written, carrying forward the reserved **header fields** and the Phase 4 surprise log — no such file. The **region table is not reserved**, so a Phase 6 region is a second `FORMAT_VERSION` break, **scheduled rather than owed** ([`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md); its queue is [`PROJECT.md`](./PROJECT.md) §5.1). The surprise-log half waits on `PHASE4.md` §1, which is not satisfiable by code. Whatever it says must reconcile with [`0009`](./decisions/0009-descoping-phase-6.md).
