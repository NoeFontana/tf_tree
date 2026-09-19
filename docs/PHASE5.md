# tf_tree — Phase 5 Implementation Specification: Offline, Observability, and the Adoption Wedge

> **Companions:** `docs/PROJECT.md` (decision log), `docs/PHASE1.md`–`PHASE4.md`.

**Deliverable:** the artifacts that make `tf_tree` useful to people who have adopted nothing.

Per D28, every user of this phase changes nothing about their robot. They point a tool at a bag, or attach a read-only process to a running system, and get something `tf2` cannot give them. That is the wedge, and it is also what produces the evidence gating Phase 7.

**Framing.** A **frozen transform index** turns a multi-gigabyte bag into a memory-mapped file queryable at native speed from sixteen dataloader workers at once, and an **observability layer** surfaces a catalogue of transform pathologies that are currently invisible. Neither requires anyone to migrate.

---

## 0.0 Implementation status

**In progress.** The live status table, in the style of `PHASE2.md` §0.0.

| Area | Status |
|---|---|
| §1 `FORMAT_VERSION = 3`, Phase 6 **header fields** reserved | **Done, for the header only.** No region slot is reserved: a Phase 6 spline region is a *twelfth* region and needs another `FORMAT_VERSION` ([`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md); §1.2). Done: header 256 → 320 with ≥ 64 bytes still reserved (asserted); the two counter regions; Phase 6's four header fields, declared absent; `nominal_rate_mhz` and `declared_by_slot` in `EdgeRecord`, `frame_kind` in `FrameRecord`; `layout_hash` `0x9075_90F5` → `0x3D10_4195`; `doctor --explain-version`. |
| §2 Frozen arena (`.tft`) | **Done.** `tf_tree_arena::frozen` writes and maps the container; `Tree::open_frozen`/`Tree::freeze_to` and `tf_tree freeze --from-live` are wired. §2.1's bit-for-bit claim is tested (`crates/tf_tree/tests/frozen.rs`). §2.4's read path is gated (§12 criterion 2, `just gate2`). |
| §3 Bag ingestion | **Partly done — MCAP only.** `tf_tree_ingest` is a workspace member (not in `tf_tree_core`/`tf_tree_arena`, not in `tf_tree_cli`, because §4's Python API needs the same logic); its consumer is `tf_tree.ingest_bag` ([`0046`](./decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md)). §3.1's two passes with spill-to-run-file and §3.3's MCAP source are implemented and gated by `cargo nextest run --workspace`, as is every §3.2 row **except `--on-clock-reset=split`**, which is refused with `IngestError::ClockResetSplitUnsupported` (§3.2's amendment). §3.1's pass one does **not** detect a time domain: every ingested edge is `SystemDomain` (§3.1's amendment). A `.db3` is diagnosed as rosbag2-sqlite3 and refused (§3.3). `freeze_from_arrays` is absent (a `tf_tree_py` entry point, no format change). `--max-memory` bounds pass two's sort buffers including the sort's scratch, **not** the arena (`ingest::fill`, `tests/memory.rs`). Zstd and lz4 chunks decode with pure-Rust codecs behind the default-on `compression` feature; `just ingest-check` builds the codec-free configuration. Decompression is bounded three times before allocation: `uncompressed_size` (64 MiB), expansion ratio (1024×), and the zstd window. A truncated recording is read up to the cut and reported as truncated. A record the reader does not read is stepped over and counted as `oversized_records_skipped`, but only if the landing position looks like a record boundary, otherwise `RecordTooLarge` (`a_skip_that_lands_on_a_later_boundary_loses_transforms_and_says_so`); a `Chunk`, `Schema`, `Channel` or `Message` over the ceiling refuses. `Survey::static_conflict_details` carries one row per contradicted edge (both poses at full `f64` precision). The report schema is `tf_tree.ingest/2` (`filtered_channels`, `non_cdr_channels`). §11's anomaly corpus is `crates/tf_tree_ingest/tests/anomaly_corpus.rs` (whole JSON byte for byte). Every §3 test reads a recording this crate wrote (`tf_tree_ingest::fixture`); no committed file is a real rosbag2 bag, and `testdata/zstd_conformance.mcap` is decoder evidence only. §12's throughput gate is gated (`just gate5`, [`0050`](./decisions/0050-what-ten-times-real-time-divides.md)). |
| §4 Offline Python API | **Done, including all three of §4.4's deltas**, and `tf_tree.ingest_bag(path)`, whose `Tree.source` carries the recording's path and BLAKE3 digest ([`0046`](./decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md)); `Tree.source` is dropped by `publisher()`. There is no `freeze_bag`: `ingest_bag(p).freeze(out)` is the composition. `tf_tree.open_file()` returns the ordinary `Tree`; `Tree.freeze()` is the way out. Of §4.2's helpers only `span` is API. §4.4 item 2 (`tree.frames()`/`tree.edges()`/`plan.edges()`, `API.md` §6 row 8) refuses a tree inherited across `fork()`; `Tree.__repr__` prints `detached-by-fork` instead. Items 1 (`Layout::QuatTwist`; `layout=` on `at`/`at_into`, `interp=` on `build`/`open(create=...)` defaulting to `"sclerp"`) and 3 (`from_parts`/`from_ros`; `tft_stamp_from_parts`/`tft_stamp_from_timespec`, ABI minor 3 → 4) are `API.md` §6 rows 7 and 9, with one refusal table asserted on both sides. Gated by `just py-test` and `just py-test-freethreaded`, and `tests/python/test_ingest.py`. |
| §5 Diagnostic counters | **Done**, §5.6 included. §5.4's NORMATIVE requirement for a *long-lived per-thread* `Guard` is **WITHDRAWN (2026-09-09)**; the convenience path keeps its per-call `Guard` (§5.4's amendment; `just guard-cost`). §5.7's measurement (`counter_cost`) shows no measurable contention at or below the CPU count, so sharding is not justified. |
| §6 Diagnostics catalogue `TFT001`–`TFT019` | **Partly done.** All nineteen ids exist and are reported; ids are appended and never renumbered. `--json` (schema `tf_tree.doctor/1`), `--exit-code` and `--suppress` are wired. **Seventeen detect** — `TFT001`, `TFT004`–`TFT019`. `TFT002`/`TFT003` detect nothing in any configuration and say so: on `--from-bag` `TFT002`'s state is in `doctor`'s own process but dropped, and `TFT003` is an `IngestError::EdgeKindChanged` that aborts before the catalogue runs; §12 criterion 6 states what each would need (a decision record). `TFT004` reads `ClaimRecord::clock_offset_nanos` ([`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md)) and fires only past a bound no publish pipeline could account for. `doctor --from-bag` and `--from-file` hand the catalogue an ordinary `Tree`. `TFT018` skips on every arena source; `TFT019` inherits that skip, and skips when the edges are in no wall-clock domain (per tag; a `SimDomain` edge is sent to [`0012`](./decisions/0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md)). "Concentrated in a short window" is at least eight consecutive rejected arrivals (`checks::CLOCK_STEP_MIN_REJECTED_RUN`). The conditional skips are not tallied here; `rg -n 'CheckOutcome::skipped' crates/tf_tree_cli/src/checks.rs` is the instrument. `TFT007` skips when no edge is comparable (no declared rate, too few intervals, or every declaring edge stopped); a `pass` always means at least one edge was compared. `TFT007` and `TFT008` withhold on stopped publishers (`checks::stopped_publishers`; §6's amendment). `TFT013` has its grace period, from `(head - 1) x median period`. `TFT016` reads `transparent_hugepage/shmem_enabled` as well as `enabled` (§2.3's amendment). §11's `doctor --json` row is met; §12 criterion 6 is not. |
| §7 `tf_tree top` | **Done, both halves.** `tf_tree top` attaches read-only, *refuses* `--rw`, and renders §7's four panes in plain ANSI (no `ratatui`). `--web` serves the same `Sampler` over a hand-rolled HTTP/1.1 loop on `std::net::TcpListener` (127.0.0.1:8787; `/api/tick`, schema `tf_tree.top/1`; no CDN, enforced by a `default-src 'none'` CSP). Gated by the unit tests in `src/web.rs` and `crates/tf_tree_cli/tests/web.rs` (also under `just shm-check`). **Not done:** keep-alive; it caps at 64 connections; no key handling. |
| §8 Visualization | **Deliberately not built** — this is the finished state, not a gap |
| §9 Benchmark artifact | **Partial.** `just bench-report` emits `report/{results.json,index.html}` with the §9.3 provenance header, every §9.2 row and all four §9.3 "where we are worse" entries; the row count belongs to `report::tests::the_required_row_set_is_the_size_of_phase5_section_9_2s_table`. Every comparison row is `UNAVAILABLE` with its own reason on this host, which is §9.3's prescribed output. `Report::validate` enforces bullets 2, 3, 4 and the enforceable half of 1 and 5; the halves it cannot reach are in its doc comment. Stale `UNAVAILABLE` reasons fail validation through a re-derived `Ground`; `MeasuredElsewhere`, `NoInstrument` and `MeasurementRefused` are not decidable there. The provenance header records both `transparent_hugepage/enabled` and `shmem_enabled` (`REQUIRED_FACTS`). **Not done:** §9.1's container image, the public sample recording, `tf_tree bench compare` (`tf_tree_bench` is `publish = false` and carries `criterion`; a decision record), and §12 gate 7. §9.1's measurement exists (`just dds-bench`, `ros/tf_tree_bench_ros`; four arms including one bridge filling a shared arena, [`0015`](./decisions/0015-the-bridge-fills-a-shared-arena.md)); its figures live in `docs/benchmarks/tf2.md`. The exploratory suite (`just contended-scaling`, `scale-sweep`, `soak`, `bench-run`/`bench-ab`) emits `tf_tree.bench-run/1` and is not gated. |
| §10 Open-source readiness | **Partial.** Name recorded ([`0008`](./decisions/0008-the-name-tf-tree.md)): crates.io `tf_tree`; PyPI distribution `transform_tree`, module `tf_tree`. Licences, `NOTICE`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md` and `SUPPORT.md` are in place. MSRV is **1.87**, built `--locked` in CI; `just msrv` checks it. Every `publish = false` crate states its reason in its manifest. The benchmark artifact is a regression gate (`just bench-check`, CI job `bench-gate`): `bench_report --check-baseline` fails on a withdrawn claim, a dropped §9.2 row, a changed `layout_hash`/`format_version`/build profile, or a directional metric past the baseline's slack; host facts in the header are ignored except the page-quantised Pss delta, banded by `RESIDENCY_SLACK` (300%). `results.json` schema is `/2`. `arena_memory_floor`'s `idle_arena_resident_bytes` is gated ([`0021`](./decisions/0021-the-idle-arena-is-resident-because-of-its-alignment.md)); `Report::validate` applies the direction rule to §9.3 entries on any host whose memory axis passes. `docs/PHASE2.md` §11.4's `shm_torture` exists (`just shm-torture`, `shm-torture-asan`, `shm-torture-self-test`); killed processes are joiners, never the rendezvous owner. Sanitizers: `just tsan`, `just shm-torture-asan`, `just cpp-check`; there is no Rust UBSan row (`rustc -Zsanitizer` has none). Release automation exists: `release.yml` publishes the five crates by Trusted Publisher and `wheels.yml` the wheels (with PEP 740 attestations), both on `v*`; `release.yml` builds four Linux archives through `just release-archive` (binary executed and version-checked, archive re-run through the `tft` symlink, licence texts included, byte-deterministic packaging) and attaches them with `SHA256SUMS`. `cargo-dist` is not used. `scripts/sbom.py` writes CycloneDX 1.5 from the shipped graph; `just lint` runs it; `release.yml` attaches it, and that step has never executed. Signed tags: `release.yml` warns, and refuses when `REQUIRE_SIGNED_TAGS` is `true`. **Outstanding:** the mdBook site; a signing key; §10's first-five-minutes path (`pip install transform_tree`), which nothing executes ([`0052`](./decisions/0052-the-first-five-minutes-nobody-runs.md), `draft`). **Declined:** `license headers` ([`0051`](./decisions/0051-the-licence-travels-with-the-artifact-not-the-file.md)). |
| §11 Test plan, §12 Gate | **Partial.** §11's **No network** row is done: `just no-network` traces the five published crates' test binaries under `strace` and asserts every `socket(2)` names `AF_UNIX` (§5.1's amendment; `scripts/no-network.sh`'s PROVES / DOES NOT PROVE header). §12 criterion 4 is met at 1.024× and gated; its Python arm reports and exits 0 by its amendment's decision. §12 criterion 5 is met and gated (`just gate5`, [`0050`](./decisions/0050-what-ten-times-real-time-divides.md)); §11's `--json` schema validation is met. **Not done:** criterion 7 (reproduce the artifact from a published container), which is §9's gap; §11's per-check fixtures and `doctor` rows, which are §6's. |

### What this development environment can and cannot gate

- **ROS 2 is available, in a container** (`tf_tree/tf2-bench:latest`, `FROM ros:lyrical-ros-base`, with `rosbag2_cpp` and MCAP storage), so §3 can be tested against real recordings. MCAP's Rust reader needs no ROS.
- **The `mcap` crate is taken with `default-features = false`**: its defaults vendor C through `lz4-sys`/`zstd-sys`, violating `PHASE2.md` §2's no-C-build-step rule. `LinearReaderOptions::with_emit_chunks(true)` hands chunks over whole and `tf_tree_ingest` decodes them with pure-Rust `ruzstd` and `lz4_flex` behind the default-on `compression` feature, so rosbag2 and Foxglove recordings ingest transparently. `IngestError::CompressedChunk` survives for a codec outside the MCAP specification and for `--no-default-features` builds (remedy: `mcap compress --compression none`).
  `IngestOptions::max_chunk_uncompressed_bytes` (64 MiB) and `max_chunk_expansion_ratio` (1024) are enforced before allocation, since neither codec crate bounds total output. A truncated **compressed** chunk is unrecoverable and reported as truncation. zstd is checked against a committed fixture from real libzstd 1.5.5 (a whole recording); lz4 against an 82-byte frame authored from the specification (`crates/tf_tree_ingest/testdata/ATTRIBUTION.md`). [`SUPPORT.md`](../SUPPORT.md#msrv-policy) records why the MSRV is 1.87. `just ingest-check` compiles and tests the codec-free configuration on `tf_tree_ingest` and `tf_tree_cli`.
- **No GPU**, so anything touching device memory stays untested.
- **CPython 3.12.3 on the host**; `uv` fetches 3.14/3.14t for `just py-setup`.
- **The benchmark host cannot fairly run §9's comparison**: 4 physical cores with SMT and `perf_event_paranoid=4`; Phase 1's read-scaling gate already fails on it. §9.3 prescribes the response: omit a row that cannot be measured fairly and say why. `tft_16_workers_rss` is `Sensitivity::Memory` and measurable here (`just gate4`). `tft_open_vs_bag_parse` is `Sensitivity::AbsoluteTiming` on a host that fails the timing axis, and no artifact computes the comparison (`just gate2` times the open, `just gate5` an ingest, on different inputs), so its row stays `unavailable`. §12 criterion 2's open time alone is measured by `just gate2`, outside the report, under §9.3's one-sided-budget amendment.
- **CI** runs again since 2026-08-16. Every gate is still run locally through `just`, with the arch stated.

---
## 0. Scope

### In scope

| | |
|---|---|
| `FORMAT_VERSION = 3` | one deliberate layout break, with room reserved in the **header** for Phase 6 (§1) |
| Frozen arena (`.tft`) | the arena bytes as a file; mmap, share, query (§2) |
| Bag ingestion | MCAP and rosbag2 → arena or `.tft`, two-pass, out-of-order tolerant (§3) |
| Offline Python API | identical to the online API, plus dataset helpers (§4) |
| Diagnostic counters | consumer-side failure counters, always on and free; publish-side derived (§5) |
| Diagnostics catalogue | `TFT001`–`TFT016`, each with a detection rule and a severity (§6); §6's amendments append `TFT017`–`TFT019`, so the shipped catalogue is **19** |
| `tf_tree top` | TUI plus an embedded static web view (§7) |
| Stored-sample iteration | `iter_edge` / `iter_edges` / `frame_path` — audit and export, viewer-neutral (§8.3) |
| Benchmark artifact | one command, reproducible, honest, CI-gated (§9) |
| Open-source readiness | the checklist that has to be true before publishing (§10) |

### Out of scope — NORMATIVE

| Excluded | Why |
|---|---|
| `tf2_ros::Buffer` shim | Phase 7, gated on this phase's evidence |
| Arena → `/tf` egress | Phase 7 |
| Splines | Phase 6 — §1 reserves their header fields |
| Covariance, CoW branches | **Cut** by [`0009`](./decisions/0009-descoping-phase-6.md) — not deferred |
| Inter-host replication | Phase 8 |
| Compression in `.tft` | Breaks `mmap`. Revisit only with a block-oriented design and measured demand. |
| A web *framework* | §7. An embedded static page and one JSON endpoint. Nothing that needs npm. |
| **All visualization work** | §8. The user's bag already opens in Rerun or Foxglove. |
| A viewer channel, plugin, or SDK dependency | §8.1. The missing-transforms case is a Phase 7 egress-bridge problem (§8.4). |

---

## 1. `FORMAT_VERSION = 3` — break it once, deliberately

### 1.1 The honest finding

Phase 5 needs new arena regions. Phases 1–3 are implemented, so this is a real break: every participant must be rebuilt and restarted together, and no version-2 arena may be attached (Phase 2 runbook).

Do it once, now, with room reserved, rather than three times across Phases 5, 6 and 8. The reservation is header bytes only; a Phase 6 region is a region and none is reserved, so a second break is owed ([`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md) names it and keeps the ledger).

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

Header fields whose Phase 6 content does not exist yet are declared with offset `0`, meaning absent. The **region table** reserves nothing: `crates/tf_tree_arena/src/layout.rs` declares `N_REGIONS` regions and none is a spline region, so a Phase 6 spline region is a *twelfth* region and changes `ArenaLayout::total_size()` — another `FORMAT_VERSION`. `spline_region_off` is a place to write an offset, not a reservation of the bytes it would point at ([`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md) part 2 is the queue a byte joins).

**NORMATIVE:** recompute `layout_hash` and bump `FORMAT_VERSION` to 3 in one commit. Ship `tf_tree doctor --explain-version`, which prints both versions and the required action when a mismatch is detected.

> **Amendment — the header grows to 320 bytes.** `ArenaHeader` was 256 bytes with 48 free (`_reserved: [u8; 8]` at 128 plus 40 bytes of alignment padding at 152..192), pinned by `header.rs` (`size_of == 256`) and `instance_uuid_occupies_pre_existing_alignment_padding`. The new header fields plus ≥ 64 reserved bytes need ≥ 77. Consequences: `topo_lock` moves off offset 192 (rewrite that test rather than delete it); the header literal in `layout.rs` changes; `layout_hash` changes automatically, since `size_of::<ArenaHeader>()` is its first input. Reclaiming the descoped covariance bytes would move `spline_region_off` off 168 and `layout_hash`, which hashes region strides and not header fields, would not catch the disagreement between two v3 participants.
>
> `layout_hash` does **not** cover region offsets, region count, or `max_frames`/`max_edges`/`max_participants`; its `strides` input is a `[u32; 12]` to which the two counter regions' strides are appended, so the hash describes the layout that exists. The regions exist whether or not the `counters` feature is compiled in (§5.5, D34), so builds with and without it have identical layouts and attach to each other. The `0x9075_90F5` literals in `tf_tree_ipc`'s wire tests are fixture values and do not move with the hash.

### 1.3 Publish-side counters need no storage at all

Adding push counters to `EdgeTelemetry` costs ~5–10 ns per ~50 ns push, a 10–20% regression on the hottest write, to store something already present:

- **push count** = `EdgeRecord::head`, a monotone counter of every sample ever published.
- **rate, jitter, gaps** = derivable from the contiguous stamp array.
- **last publish time** = the newest stamp.

The entire publish-side diagnostic surface is computed by a reader walking existing data; the push path is untouched. Only consumer-side failures need storage, and those increment on error paths.

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

`ParticipantCounters` mirrors it per participant slot, so `doctor` can say *which consumer* is failing.

---

## 2. The frozen arena — `.tft`

### 2.1 The file *is* the arena

Phase 1 invariant 2 says no pointers in the arena; every internal reference is an offset, so the arena is relocatable by `memcpy` and can be written to disk and mapped back with no parsing or fixups. A frozen `.tft` is a header, a manifest, and the arena bytes. Opening one is an `mmap`.

**NORMATIVE:** the frozen read path uses the **identical** `Plan::at` code as the online path, against a `PROT_READ` mapping. No offline variant of the lookup, no separate index. The bit-identical replay test from Phase 2 §10 extends to `HeapArena` / `MappedArena` / `FrozenArena`.

### 2.2 Why this is the wedge

A perception team's dataloader re-parses the bag in every worker, precomputes poses into a pickle (losing arbitrary-time queries), or runs a ROS node during training. With a `.tft`, sixteen workers each `mmap` the same file, the kernel shares one set of clean pages, and each worker queries at ~50 ns with no IPC; marginal RSS per worker is approximately zero (Phase 2 §3.8's page-sharing argument). §9 carries the benchmark row: total RSS across 16 workers versus 16 independent bag parses.

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

`arena_off` is **2 MiB aligned** so the mapping is eligible for transparent huge pages and `MADV_HUGEPAGE` is meaningful. A huge page can back a mapping only where virtual address and file offset are congruent modulo 2 MiB; the offset is the only half this format controls, so `MADV_HUGEPAGE` is best-effort and its failure is not an error.

> **Amendment — the huge-page benefit is unverified on any host we have.** `cargo run --release -p tf_tree_bench --features shm --example hugepage_grant` measures the grant (`ShmemPmdMapped`/`AnonHugePages`/`FilePmdMapped` in `/proc/self/smaps`). On the development host (an EPYC-Milan guest) a fully resident 72 MiB arena is granted 0 KiB, also after setting `shmem_enabled` to `advise`; the `memfd` mapping is 2 MiB aligned anyway, and `/proc/vmstat` shows `thp_file_alloc 0` and `thp_file_fallback 0`, so the kernel never attempted an allocation. The TLB-reach claim (a 115 MB index needs ~28 000 entries on 4 KiB pages, 55 on 2 MiB) is a projection, not a measurement. A live arena is governed by `transparent_hugepage/shmem_enabled`, not `enabled` (§6, `TFT016`).

The manifest is CBOR because it is cold and variable-length; everything hot is in the arena. `source_digest` makes a `.tft` traceable to its recording.

> **Amendment — the container header is 128 bytes.** The listed fields come to 120 bytes; `tf_tree_arena::frozen::FROZEN_HEADER_SIZE` pins the header at **128 with 8 reserved**, asserted by a test, so the file layout does not depend on `size_of`. The reserved tail is written zero and not checked on read, so a future field there must be optional by construction.

> **Amendment — the manifest's per-edge span is one-sided.** `newest_ns` is exact; the other end is **`oldest_ns`**, the oldest sample *still retained in the ring*, not the oldest the source contained. Both are `null`, not `0`, for an edge that has never published.

> **Amendment — the per-edge sample count is two keys.** `EdgeRecord::head` counts every sample ever pushed; the file holds `min(head, retained)`. The manifest emits **`samples`** (what the file holds) and **`pushes_total`** (what the source produced); their ratio is how much the ring dropped during ingest. The per-edge map is eight keys.

### 2.4 Read path

```
fstat, pread FROZEN_HEADER_SIZE bytes at 0
validate FrozenHeader (magic, format_version, layout_hash, file_size)
check_extents  (arithmetic on the header's own offsets and lengths)
mmap(arena_off, arena_size, PROT_READ, MAP_PRIVATE | MAP_NORESERVE)
madvise(MADV_HUGEPAGE)                    // best effort
validate_arena_header  (the mapped ArenaHeader: version, layout, magic)
```

`validate_arena_header` (`crates/tf_tree_arena/src/check.rs`) dereferences the mapped base, so an open touches exactly one page of the arena; §12 criterion 2's evicted arm uses that as its witness (one major fault when the eviction took, zero when it did not). There is **no checksum, no name-table walk, no manifest decode and no `source_digest` verification** on this path; every step is O(1) in the index size, which is what §12 criterion 2 gates.

`MAP_PRIVATE` on a read-only mapping still shares clean page cache and removes accidental writeback. No socket, lock file or participant table: a frozen arena has no writers. `AttachMode` is permanently `ReadOnly`.

**NORMATIVE:** `layout_hash` mismatch is a hard error naming both values and stating that the file must be re-frozen. A `.tft` is a cache, not an archive; keep the source recording.

### 2.5 Sizing

Freezing computes exact per-edge capacities from a counting pass, so there is no wrap and no wasted space. A 30-minute recording with one 1 kHz edge, four 200 Hz edges, and twenty static edges:

```
1 kHz  × 1800 s = 1.8 M samples × 64 B  = 115 MB
200 Hz × 1800 s × 4 edges = 1.44 M      =  92 MB
stamps                                   =  26 MB
                                          ------
                                          ~233 MB
```

Binary search over 1.8 M samples is ~21 probes against ~12 for an online buffer, over a dense stamp array. No special indexing is needed; resist adding one until a benchmark says otherwise.

---

## 3. Bag ingestion

### 3.1 Two passes — NORMATIVE

**Pass 1 (count).** Scan the recording; per edge, count samples and record `[t_min, t_max]`; collect frame names; detect edge kind (dynamic vs static) and time domain. Output: an exact `ArenaLayout`.

**Pass 2 (fill).** Re-read, group by edge, **sort by stamp within each edge**, then push in order. Sorting is required because Phase 1 invariant 6 mandates non-decreasing stamps per edge and a recording is ordered by log time, not header time.

Memory: buffer per edge, sort, drain. Peak is roughly the dataset size (~233 MB above). **Cap it** at a configurable `--max-memory` (default 4 GiB) and spill to a temporary run-file with a k-way merge beyond that.

> **Amendment — the cap is enforced by two mechanisms.**
>
> 1. **Grouping.** Edges are partitioned into sets whose buffers fit the cap together, and the recording is re-read once per set. This handles every recording whose largest *single* edge fits and leaves no temporary file; it is preferred.
> 2. **The run file**, for one edge over the cap on its own: spill cap-sized sorted runs, then merge. A k-way merge holds at least one sample of every run resident, so a single-pass merge over more runs than `cap / 64` exceeds the cap by construction; runs are therefore *reduced* in passes, merging a bounded fan-in at a time into a fresh file, until one merge can hold what is left.
>
> Ties break by run index and runs are merged in contiguous windows, so §3.2's "last occurrence wins" survives across passes (`a_reduce_pass_keeps_the_last_occurrence`).
>
> `--max-memory` bounds the sort buffers and not the arena (`ingest::fill`'s doc comment; `tests/memory.rs`). The spill path's run index (16 B per run) is also outside the cap and is reported as `FillStats::peak_run_index_bytes`, beside `spilled_bytes`.

> **Amendment — the stable sort's scratch is inside the cap.** §3.2's "last occurrence wins" needs a stable sort, which may allocate up to one extra copy of the buffer it sorts. The group reserve is its **largest member**: the peak of a group is `sum(buffers) + max(scratch)`; a spill run is sized against `2 × ENCODED`; `FillStats::peak_buffer_bytes` includes the term. The reserve is an upper bound because the standard library does not promise what a stable sort allocates. Cost: a group holds one edge fewer, an edge between `cap / 2` and `cap` takes the spill path, and a spill run is half as long. §12 gate 5's cap is derived from the survey by `grouped_cap_from`, which carries the same reserve. What is gated is the arithmetic — `ingest::tests::groups_respect_the_cap` (`sum + max <= cap`) and `spill::tests::budget_fits_the_cap` (`2 × run × ENCODED + staging <= cap`); `tests/memory.rs` asserts the report, not the process, because no instrument in the workspace measures an allocator. A non-allocating sort (`sort_unstable_by_key` over an arrival index, 4-8 B per sample instead of 64) would move `spill::ENCODED` and is a decision record.

> **Amendment — pass one does not detect a time domain.** `tf_tree_ingest` produces per-edge counts, `[t_min, t_max]`, frame names and edge kind, and **no domain**: `ingest::fill` is the only `TreeBuilder` call site (`rg -n 'TreeBuilder::new|static_edge|dynamic_edge' crates/tf_tree_ingest/src/ingest.rs`) and every ingested edge takes the default, `SystemDomain` (tag `0`). This is a gap rather than closed in code because the clause has no defined input: a `TFMessage` and the MCAP records around it name no clock, `use_sim_time` is not recorded in a bag, and a `/clock` topic is a topic name, which §3.3 deliberately does not use for discovery. Every other domain in the project is **declared**, never detected (`tf_tree_bridge::config`, `EdgeCfg::domain`, `TreeBuilder::default_domain`). Cost ([`0038`](./decisions/0038-the-domain-a-binding-cannot-name.md)): an arena of simulated stamps tagged as the system clock never raises `LookupError::TimeDomainMismatch`, and `source_digest` does not distinguish the clock. Closing it is new public API on two crates (an `IngestOptions` field, `tf_tree ingest --time-domain`, and `freeze --from-bag`) and belongs in `docs/decisions/`.

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

The ingest report is a first-class output: emit it as JSON alongside the `.tft` and summarize it to the terminal.

> **Amendment — frames and edges are declared in canonical (name-sorted) order, not first-seen order.** §11 requires that shuffling a recording's messages produce an identical result; ids are assigned in declaration order, so first-seen order gives different `FrameId`s, `EdgeId`s, ring offsets and `LookupError::Extrapolation { edge }`. The arena is a pure function of the recording's content and the ingest report is diffable between runs.

> **Amendment — the clock-reset threshold.** `tf_tree_bridge`'s `ClockGuard` threshold is reused so a recording and a live system draw the same line, but a backward stamp is **dropped online and kept offline**: offline §3.1 sorts, so discarding it would throw away what the sort recovers. Backward jumps below the threshold are counted as `out_of_order` and kept. The threshold is meaningful because a recording is written in log order, so §11's shuffle test must raise it to run at all.

> **Amendment — the guard is per edge.** One `ClockGuard` over every edge halts at the defaults on ordinary topologies: `map -> odom` is stamped at the scan and published hundreds of milliseconds later, and two robots on `/robot1/tf` and `/robot2/tf` differ by their clocks. That is a difference between publishers' latencies, unbounded, so no threshold fixes it. Monotonicity is a per-edge rule (§3.1 sorts per edge; invariant 6), and a bag loop or sim reset moves every edge at once. The error names the edge.

> **Amendment — `--on-clock-reset=split` stays refused; this is the decision, not the backlog.**
>
> 1. **The output type changes everywhere.** Downstream of an ingest, `--out` is a path, §2.3's container holds one arena, `open_file()` returns one `Tree`, `tf_tree top` attaches to one, and `source_digest` identifies one recording. `split` would turn `--out` into a template and the report into a set.
> 2. **There is no container for a segment set.** After a reset segment stamps *overlap*, and an arena has one time axis per edge (invariant 6).
> 3. **The user almost never wants N of them.** A backward jump is a bag loop, a sim reset or concatenated recordings; `halt` reports the edge, stamp and magnitude needed to cut the recording with `mcap filter` or `ros2 bag convert`.
>
> The variant stays spelled: the parser accepts it and refuses with `IngestError::ClockResetSplitUnsupported`, pointing at this amendment, so a typo is distinguishable from "not built". It would reopen for a user who needs every segment indexed and cannot cut the source.

### 3.3 Sources

- **MCAP** — primary. Read `tf2_msgs/msg/TFMessage` via the schema, not by assuming a topic name; support `/tf`, `/tf_static`, and remapped equivalents.
- **rosbag2 sqlite3** — lower priority; convert to MCAP where practical.
- **A running arena** — `tf_tree freeze --from-live --duration 60s` snapshots a live system.
- **Python** — `tf_tree.freeze_from_arrays(...)` for users whose poses are not in a bag at all.

> **Amendment — the rosbag2 sqlite3 source is blocked on the dependency budget, measured.**
>
> | Candidate | Verdict |
> |---|---|
> | `rusqlite` / `libsqlite3-sys` | Vendors C, against `docs/PHASE2.md` §2's no-C-build-step rule. |
> | `prsqlite` 0.1.0 | Pure Rust, but the crates.io index records no licence, so `cargo deny check` refuses it. |
> | `sqlite-rs` 0.3.7 / `sq3_parser` 0.3.3 | Header- and pager-level parsers; neither demonstrates table-row or BLOB iteration. |
> | Write one here | A b-tree walk, varint records and overflow-page chains, in a crate whose reason to exist is not reimplementing databases. |
>
> The remedy is §3.3's own: `ros2 bag convert`. What is built is the diagnosis: `tf_tree_ingest::source::is_sqlite` checks the sixteen magic bytes and returns `IngestError::Rosbag2Sqlite`, and the CLI prints the conversion command. The fixture is a real SQLite database with rosbag2's schema (`testdata/rosbag2/`). It reopens for a permissively licensed pure-Rust SQLite reader that iterates rows and reads BLOBs.

---

## 4. Offline Python API

### 4.1 Identical to online — NORMATIVE

```python
ds = tf_tree.open_file("run.tft")            # mmap, microseconds
plan = ds.plan("map", "lidar")
Ts = plan.at(stamps)                          # the same call as online
```

`plan`, `at`, `at_into`, `adaptive`, `latest` — the same objects, semantics and bit-exact results. **Do not introduce a parallel offline API.**

### 4.2 Dataset helpers

Additions that only make sense when the whole timeline is present:

```python
ds.span("map", "lidar")            # (t0, t1) over which the plan is answerable
ds.edges()                         # per-edge: rate, jitter, gaps, count, span
ds.gaps("odom", "base_link", threshold_ns=50_000_000)
ds.resample("map", "lidar", t0, t1, hz=100)      # uniform grid, vectorized
ds.manifest                        # source path, digest, ingest options, versions
```

`span` is `LatestCommon` generalized to a range: the interval over which *every* dynamic edge on the plan has data.

> **Amendment — one of these five shipped; the other four are decisions.**
>
> `span` is on `Tree` and works on a live tree too. It returns `(t0, t1)`; `(t0, t1)` with `t0 > t1` when the windows do not overlap (an empty intersection is an answer, not an error); and `None` when every step is static. An edge that has never published raises `NoDataError` **naming the edge's two frames**. The arithmetic is `tf_tree_core::Plan::span`, which takes a `Guard` and calls `check_generation`, so `TopologyChanged` and `ChildDetached` reach the caller; the binding only re-labels `NoData`. The folded-`Step::Static` branch is covered in `crates/tf_tree/tests/behavior.rs`.
>
> `resample` is `plan.at(np.arange(t0, t1, 10**9 // hz))`; a second spelling is what §4.1 forbids. `edges()` and `gaps()` need §3's counting pass (the ring knows what it *retained*, not what the source produced; §2.3's `samples`/`pushes_total` amendment), and `manifest` needs a CBOR *reader*, where the crate has only a writer.

### 4.4 Three API-contract deltas that land here — NORMATIVE

[`API.md`](./API.md) §6 rows 7, 8 and 9: each is a gap between Python and a surface that already has the feature.

**1. `Layout::QuatTwist` — derivatives reach Python and C (`API.md` §3.3).** Python had no path to a twist. It ships as a **fourth `Layout` variant**, not a fourth method: a contiguous `(N, 13)` write of `[qw qx qy qz tx ty tz | ωx ωy ωz vx vy vz]`, carried to both bindings by the layout dispatch that already exists. `LerpSlerp` returns `DerivativesUnavailable` here as it does from `at_with_derivatives`. On the C side it is one new `tft_layout` enumerator and a **minor** ABI bump (`PHASE4.md` §3.6).

> **Status: done, on all three surfaces.** `Plan::at_many_into` serves it; `TFT_LAYOUT_QVEC7_WXYZ_TWIST6` is accepted by `tft_plan_at` and `tft_plan_at_many` and reachable from C++ as `layout_of<Quat7Twist6>`. `tf_tree_py` takes a keyword-only `layout=` on `at` and `at_into` (`"mat4"`, `"quat"`, `"affine32"`, `"quat_twist"`); `LerpSlerp` raises `DerivativesUnavailableError`. `build` and `open(create=...)` take `interp=`, and the default moved to `"sclerp"` because `PROJECT.md` §5 D5 forbids a `LerpSlerp` default without a measurement (`API.md` §3). `PHASE3.md` §6.1's amendment is the single account of `NS_PER_STEP_ESTIMATE`.

**2. Introspection: `tree.frames()`, `tree.edges()`, `plan.edges()` (`API.md` §3.2).** Notebook users otherwise shell out to the CLI. `tree.edges()` is the *names* half only and is different from §4.2's `ds.edges()`; do not let it acquire statistics, because a rate computed from a ring is the error §4.2 refuses.

**3. Exact stamp converters: `from_parts` / `from_timespec` / `from_ros` (`API.md` §5.1).** `from_sec` is lossy above 10⁷ seconds; the fix is an exact, total converter on every surface, none taking a float. `tf_tree.from_ros` converts a `builtin_interfaces/Time` exactly and **never** via `to_sec()`.

> **Status: done** (`API.md` §5.1's amendment). All three refuse rather than normalise or wrap; `tests/python/test_api.py`'s `PARTS_TABLE` and `crates/tf_tree_c/tests/abi.rs`'s `PARTS_TABLE` are the same ten rows asserted independently on each side. Python has no `from_timespec`, since `time.clock_gettime_ns()` is already integer nanoseconds; C has both.

### 4.3 The dataloader pattern

Document it, do not ship a class: a `torch.utils.data.Dataset` subclass would bind us to a framework version.

```python
class Frames(Dataset):
    def __init__(self, path): self.path = path; self.ds = None
    def __getitem__(self, i):
        if self.ds is None: self.ds = tf_tree.open_file(self.path)   # per-worker, post-fork
        ...
```

> **Amendment — the reason for the lazy open.** Phase 3's fork poisoning does **not** apply to a `.tft`: `Tree::from_frozen` goes through `fork_gen_for`, which returns `None` for `ArenaBacking::Frozen`, since the mapping is `MAP_PRIVATE | PROT_READ` and not `MADV_DONTFORK`, so a child inherits it intact (`tests/python/test_frozen.py` forks and queries it). The lazy open exists because **a `Tree` cannot be pickled** and a `DataLoader` with `num_workers > 0` pickles the dataset under `spawn` and `forkserver` (CPython 3.14's default on Linux); the lazy `None` keeps the object picklable. Opening per worker is also what §2.2's page-sharing argument depends on.

---

## 5. Diagnostic counters

### 5.1 "Telemetry" is the wrong word — NORMATIVE

Rename it everywhere: **counters**, or **diagnostic counters**. Never "telemetry", in the code, the docs, the CLI, or the changelog.

> **Amendment — this is an enforcement item.** `telemetr` has zero hits in code; the only occurrences are in this document and `PHASE4.md` §3.1's unstable-header table. `EdgeTelemetry` in §1.3 is a rejected hypothetical. The deliverable is a CI check that keeps the word out.

In 2026 "telemetry" means the software phones home, and some teams block on the word at procurement. Nothing here leaves the machine.

**The `tf_tree` *library* opens no network sockets. Ever.** The only socket in the library is the Phase 2 `AF_UNIX` rendezvous socket. Phase 8 replication will be an explicitly enabled, separately named component.

**`tf_tree` is also the shipped binary, and it has exactly one exception: `tf_tree top --web`.** It binds an `AF_INET` listener only when an operator types the flag, loopback unless they name another address (§7's amendment). Nothing a program links can reach it.

**NORMATIVE CI test:** run the **library's** test suite under `strace` and assert that `socket(2)` is called only with `AF_UNIX`.

> **Amendment — the assertion exists.** `just no-network` (`scripts/no-network.sh`) is the assertion, run by `ci.yml`'s `shm` job on both matrix rows. "The full suite" was wrong: `tf_tree top --web` binds `AF_INET` by construction and is in the full suite, so §11's "scoped to the library's suite" is the correct wording. `scripts/no-network.sh` carries the PROVES / DOES NOT PROVE header and a RED TESTS block naming the seeded refusals.

### 5.2 The two kinds of counter have opposite cost profiles

| | Cost | Value |
|---|---|---|
| **Error-path counters** — extrapolation, no-data, recycled, contended | **Zero.** The branch is already taken and an error object is already being constructed. | Irreplaceable: you look at these *after* something went wrong at 3am on Tuesday. |
| **Success denominator** — `lookups_ok` | An atomic increment on the hottest read path, on a per-edge line shared by every reader. | Convenience: it turns an error *count* into an error *rate*. |

### 5.3 Error-path counters are always on, with no runtime switch — NORMATIVE

Diagnostics that are off by default do not exist when you need them; if enabling one requires a restart, the incident is gone. Error-path counters cost nothing, cannot affect a lookup result, and are the basis of `TFT010` and `TFT011`. No environment variable, no runtime flag.

### 5.4 The denominator batches in the `Guard`, so it is also free

A `Guard` is per-thread, scoped, and spans a batch of lookups, so accumulate in it and flush once on `Drop`:

```rust
pub struct Guard<'a> {
    // ...existing fields...
    ok: Cell<u32>,          // plain, non-atomic; Guard is !Sync by construction
}
// Drop: if ok > 0 { edge_counters.lookups_ok.fetch_add(ok, Relaxed) }
```

A `Guard` spanning 1000 lookups pays one relaxed atomic per 1000, per thread.

> **Amendment — the requirement for a long-lived per-thread `Guard` on the convenience path is WITHDRAWN; the convenience path keeps its per-call `Guard`.**
>
> It contradicts the batching argument: `Guard`'s `Drop` is the **only** thing that publishes `lookups_ok` (`crates/tf_tree_core/src/plan.rs`), while `note_err` writes error counters straight through, so a guard that never ends holds the denominator and publishes the numerator. `TFT010` computes `errs / (errs + lookups_ok)` and would read 100% on a healthy edge; `no_counter_evidence` skips every counter check when that sum is zero.
>
> It is also unsound as specified: a `thread_local!` needs a `Guard<'static>`, a **second lifetime extension**, which is a decision record ([`0017`](./decisions/0017-owned-handles-and-the-lifetime-rule.md)). `Tree::lookup` takes `&self` and `Tree` is `Send + Sync`, so the handle can drop on another thread while a cached guard's destructor still writes into freed memory.
>
> The price is `just guard-cost` (`crates/tf_tree_bench/src/backing.rs`, [`EVIDENCE.md`](./benchmarks/EVIDENCE.md)): `Tree::guard()` per call against one hoisted guard, on a single-edge plan (`Guard::drop` credits an edge only when the whole batch went through exactly one, so a multi-edge plan understates it). It does not decide this, since a requirement that breaks a shipped diagnostic is withdrawn whether it costs 4% or 16%. This host has four physical cores, so nothing here extrapolates to §5.7's sixteen readers. R2 governs the *hot* tier (`Plan::at` takes `&Guard`); a per-call `Guard` neither allocates, locks nor converts (`crates/tf_tree/src/cache.rs`).
>
> The convenience path instead credits its denominator on **every call**: `the_convenience_path_publishes_its_denominator_on_every_call` (`crates/tf_tree/tests/counters.rs`), whose per-iteration assertion is the whole test and which is mutation-verified against one hoisted guard.

There is nothing left worth switching off at runtime.

### 5.5 The one switch that should exist is compile-time

For a certified or minimal build, the knob is a **default-on cargo feature**, not a runtime flag:

```toml
[features]
default = ["counters"]
counters = []
```

Disabling it removes the fields, the increments, and the regions' *use*; the regions remain (D34), so the layout hash does not fork.

> **A read-only participant keeps no counters.** D18 makes a consumer's attachment read-only, so *any* write from a read path faults; the `Guard` flush is one, and killed a read-only child with `SIGSEGV` before the check existed. A read-only participant silently records nothing. `ArenaView` carries a `writable` flag, default `false`.
>
> The consequence: an `EdgeCounter` is incremented by a **lookup**, and a bridge publishes. `TFT010` and `TFT011` are about consumers, which D18 makes read-only, so on a publish-only publisher plus N read-only consumers those two checks see nothing. The disclosure is `no_counter_evidence`'s skip reason, which names the read-only cause and points at `tf_tree participants`. A writable counters region for read-only consumers (a second mapping or a per-participant region) is a decision record.

### 5.6 Counters are captured in snapshots

**NORMATIVE:** `tf_tree freeze --from-live` copies the counter regions into the `.tft`, and `doctor --json` output is timestamped and appendable, so a field snapshot carries the diagnosis.

> **Amendment — the counters are in the arena image, not the manifest.** §2.1 makes the file an arena image: `ArenaLayout::edge_counters()` and `participant_counters()` land at their own offsets and are read through the same `ArenaView::edge_counters` accessor. No code can forget to copy them, and there is no second source of truth. The manifest keeps what the arena cannot hold: source path, digest, ingest options.

### 5.7 What must still be measured

Publish the cost of the non-atomic `Guard` increment, and confirm under sixteen concurrent readers that flush-on-drop shows no measurable contention; if it does, shard by participant slot (`counters[edge][slot & 7]`).

> **Measured.** `cargo run --release -p tf_tree_bench --bin counter_cost`, 4 physical cores + SMT, `ns/lookup/thread`:
>
> | threads | 1 | 2 | 4 | 8 |
> |---|---|---|---|---|
> | counters on | 21.1–21.5 | 21.4–22.6 | 22.0–22.8 | 38.0–40.2 |
> | counters off | 18.5–18.9 | 18.5–20.1 | 19.1–19.6 | 34.1–36.8 |
> | ratio | 1.13× | 1.14× | 1.16× | 1.11× |
>
> The counters cost about 2.6 ns per lookup (the `Cell` increment, the `first_dynamic_edge` walk, the `is_writable` branch) and the ratio is flat, so there is no contention and **the sharding fallback is not justified**. §5.5's compile-time switch removes the 14% for a build that cannot pay it. The 16-thread row is not quoted (2× oversubscribed here). Batched against per-lookup flushing is +5.8 ns within the counters-on build, an upper bound on the atomic §5.4 removes. Builds are verified with `cargo tree -e features`: `tf_tree_core` must set `default-features = false` for the control build to be counters-off.

---

## 6. The diagnostics catalogue

`tf_tree doctor` exists from Phases 1–2; Phase 5 makes it the product. Each check has a stable identifier so it can be suppressed, tested, and referenced from documentation.

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

`TFT016`'s evidence is text-parsed from `/proc/self/limits` by `crates/tf_tree_cli/src/hostfacts.rs` (`tf_tree_cli` is `#![forbid(unsafe_code)]` with no `libc`). Its finding does not predict that `mlockall` will fail, because `mlockall` charges the whole address space and this compares a limit against the **arena**; it says its silence is not a clearance and names `MCL_ONFAULT` ([`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md)).

Output modes: human (default, coloured, grouped by severity), `--json` (stable schema, for CI), and `--exit-code[=error|warn]` so `doctor` can gate a robot's startup or a CI job.

> **Amendment — `TFT015`'s participants row is not the header's to give.** `ArenaHeader::participant_count` is never incremented and cannot be: a killed participant cannot decrement it (D17 keeps liveness in a lock byte). The arena participant table fails the same way, because a read-only attachment (D18's default) takes a lock byte and writes **no arena record** — measured at one publisher and one read-only consumer, 2 held lock bytes against 1 record. Both arena-side sources are invalid; which lock-file population replaces them is [`0056`](./decisions/0056-the-participant-numerator-is-the-lock-files.md)'s (`draft`) open question. The row is absent today and its absence is disclosed in `Meta.notes`.

> **Amendment — `--exit-code` has a `warn` tier.** On a live arena four of the six `Error` ids structurally skip (`TFT001`–`TFT003`, `TFT018`), so `--exit-code` reduced to `TFT006` and `TFT012`, while almost everything an operator is paged about is `Warn`. Bare `--exit-code` still means `error`; `warn` is *warn-and-above*. `--suppress` is the escape hatch. `Report::is_healthy` backs it.

**`TFT004` is the check most likely to find something nobody knew**: on a multi-machine robot with imperfect PTP it finds clock problems that present as intermittent extrapolation errors.

> **Amendment ([`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md)) — do not compute the offset here; it is already computed.** Differencing a stored receipt time against the ring's newest stamp reproduces a ±1 s noise floor, because the write is sampled once per second of published data. **The writer does the subtraction**: `ClaimRecord::clock_offset_nanos` **is** the per-publisher offset. Read it; do not difference it.
>
> - **`0` means *no sample yet***; an exact-zero offset is stored as `1`.
> - **Only `SystemDomain` (tag 0) edges record anything**; a `SimDomain` edge would record ~1.79 × 10¹⁸.
> - **Four skips**: `== 0`, `TFT005`'s epoch condition, a frozen `.tft`, and a **replayed** source (bag ingest publishes through the same `EdgeWriter`, so a 2024 recording records a two-year offset).
>
> **The fleet comparison is not implementable from one sample.** A recorded offset is clock error *plus* stamp-to-push latency, and one sample cannot separate them: a localiser stamping at scan capture legitimately sits tens of milliseconds above an odometry publisher. What separates them is drift, which needs a series; `crates/tf_tree_cli/src/top.rs`'s module header carries the owed rule. So `TFT004` fires only past `checks::OFFSET_BEYOND_ANY_PIPELINE_NS` (ten seconds, where latency is no longer an explanation): it finds a machine whose NTP never came up or whose RTC is dead, not PTP-scale drift. The fleet spread ships as a report note. `ros2 bag play` into a live stack defeats the replayed-source skip; the finding text names replay as the alternative reading.

> **Amendment — `TFT007` is no longer structurally blind.** The declared rate comes from the topology file: `rate_hz` -> `TopologyConfig::builder` -> `EdgeCfg::nominal_rate_hz` -> `TreeBuilder::build` -> `EdgeRecord::nominal_rate_mhz`. `FORMAT_VERSION` stays 3 and `layout_hash` does not move; the field already existed (§1.2).
>
> * **`0` means undeclared, not 0 Hz**: such an edge is not compared; when *no* edge declares one the check skips and names the knob.
> * **The observed rate is the median inter-arrival**, the statistic the `edges` column prints.
> * **Both directions fire**: fast retains proportionally less history than consumers were tuned against.
> * **A partial run says so** in `Meta.notes`. **A run that compared *nothing* skips** (no edge retained enough intervals: bringup, a publisher restart, or a stopped publisher), with a reason naming the missing stream.
>
> **A measured rate is not a declared rate.** `tf_tree topology --discover` writes a *measured* rate into the `rate_hz` the arena reads as *intended*, so a recording of a publisher degraded at 12 Hz declares 12 Hz nominal: `doctor` certifies the fault and fires when the publisher is repaired. A discovered `rate_hz` is a starting point to review; `--discover` prints each edge's sample count to stderr for that, and an unmeasurable edge is emitted as `capacity = N`. The reference fixture (`tf_tree_bench::fixture`) declares no rate, so `TFT007` is not run there; the ROS 2 bridge's arena runs it.

> **Amendment — the catalogue runs to `TFT018`: the two Phase 1 checks with no id have one.** `unclaimed-dynamic` and `out-of-order` were reported id-less. They are **appended, not folded in** (`TFT013` is *declared, never published*, not *published then abandoned*; `TFT014` is a slot or claim whose owner is gone, not *no claim at all*; `TFT006` judges a stamp's *value*). Appending is additive; ids are a public contract (`--suppress`, `--json`, runbooks), never renumbered or recycled. Severity is preserved (`TFT017` warn, `TFT018` error; `checks::tests::the_two_new_ids_keep_their_phase_1_severities`). `out-of-order` skips on a live arena because a ring being written while read can show the next lap's sample at the old end of the window. The `uncatalogued` array stays in the `--json` schema with no producer.

> **Amendment — `TFT019`: `CLOCK_REALTIME` is not monotone, and the failure reads like our bug.** ([`API.md`](./API.md) §5.3.) NTP steps and leap seconds move it backwards, which surfaces as a **burst of `NonMonotonicStamp` rejections** (`PHASE1.md` §2 invariant 6) that `TFT018` reports as "a publisher restarted without resetting its clock".
>
> **`TFT019` is an attribution, not a second detector.** It fires on `TFT018`'s evidence plus the edge's **declared domain tag** (`EdgeRecord::domain`): a run of rejections *concentrated in a short window* on a wall-clock edge is a clock step, and the publisher is not at fault. It fires only on tag 0; on any other tag it **skips with a reason naming the tag**, since a user-declared tag cannot state "this clock can step". `SimDomain` (tag 2) and `SteadyDomain` (tag 3) exist (`API.md` §2.5; `tf_tree_bridge::config::parse_domain` spells `"sim"` and `"steady"`). `checks::tag_refusal` has a dedicated arm for tag 3 (*"a steady clock, which cannot have stepped, so this is a real publisher fault"*), pinned by `checks::tests::tft019_fires_only_on_the_wall_clock_tag_and_names_the_tag_it_refuses`.
>
> - It does not fire on a steady or sim tag; sim time's harder question (a `/clock` reset against `transform_tolerance`) is settled by [`0012`](./decisions/0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md)'s authoritative `rcl` signal.
> - It does not demote `TFT018`, which stays an error; `TFT019` is a warn that explains it.
> - It does not reuse the bridge's `dropped_non_monotonic` counter (`PHASE4.md` §5.5), which is not an `EdgeCounters` field.
>
> The recommended documentation line: anything published at rate should declare a steady or PTP domain (`SteadyDomain`, or a driver's own `Domain` unit struct and `TAG`; `API.md` §2.5). `RUNBOOK.md`'s `NonMonotonicStamp` section carries both.
>
> > **Amendment — `doctor` reads recordings, and the skip is keyed on the property that decides it.** `--from-bag <recording.mcap>` ingests through `tf_tree_ingest::run` and `--from-file <index.tft>` opens a frozen arena through `Tree::open_frozen`; both hand the catalogue an ordinary `Tree`. `TFT018`/`TFT019` run on `--from-bag` and **skip on `--from-file`**: liveness was never the property, since `SampleRing::push` rejects an out-of-order stamp, so a ring (live, `.tft`, or §3.1's sorted arena) holds only accepted pushes and the rejected arrival is absent. Wired by liveness, both checks would have passed on every `.tft` ever written. The skip is keyed on `tf_tree_cli::checks::PushStream` (`Observed` and `Recorded` run; `RingsAtRest` and `RingsUnderWriter` skip with different reasons), with the predicate and the reason returned by one function. `TFT001` is re-keyed by the same enum: a recording has no sender field, so **a bag cannot answer the multi-publisher question**. The recording path replays the recording a third time through `source::read_tf` because `Anomalies::out_of_order` has no edge attached and neither pass retains arrival order.
>
> > **Amendment — `TFT010` and `TFT011` skip on evidence, not source.** An arena built from a recording has been written and never read, so every `EdgeCounters` field is zero, which is what a healthy heavily-exercised arena looks like; `TFT010` reported `pass`. Both now skip, via `tf_tree_cli::checks::no_counter_evidence` (`lookups_ok + err_extrap_before + err_extrap_after` summed over the arena, so a new source cannot reintroduce the bug). The reference fixture publishes and never looks up, so `TFT010` skips there too and `tf_tree doctor` reports `11 passed, 2 fired, 6 not run`. A live arena at bringup gets the same skip. The threshold is *any* lookup. The `counters`-feature skip keeps a separate reason. `TFT011` skips only when *both* halves are blind; the surviving half is disclosed in `Meta.notes`. **`TFT017` is deliberately not skipped**: a bag-built arena has no writer, and a fleet whose publishers all died reaches the identical state; an all-unclaimed arena earns a `Meta.notes` line instead. Whether a `Recorded`/`RingsAtRest` source should suppress it is an open question; the naive "no edge carries a claim" would silence the total-outage case.

> **Amendment — `TFT014` detects the participant-slot leak; nothing is reclaimed.** The claim half was blind in the state the other half is about: it fired on `owner_pid == 0`, which came from `ParticipantTable::identity`, which answers for any record whose `state` reads `LIVE`, which a participant killed without `Drop` leaves behind (`PHASE2.md` §5.1: "any code deciding liveness from `state` is a bug"; [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) step 6).
>
> **Which arenas the slot half fires on.** The owner's socket-hangup reap releases a `SIGKILL`ed rendezvous joiner's record, so a slot finding means the slot is one the reap cannot reach: the owner's own slot, an owner killed between the hangup's probe and its CAS, a client the owner's `epoll::add` failed for, a takeover heir's inherited peers, or a byte-less `TreeBuilder::build_shared` creator ([`0031`](./decisions/0031-the-participant-record-with-no-byte.md): serving such an arena is *out of contract*, and `TFT014` **accuses** it). `0028` step 0b made both fd-attach arms refuse `ReadWrite`. What `--attach` can reach is a property of the arena at run time: a dead owner's rendezvous refuses a join with `ArenaHeldButUnreachable`, and `Tree::inherit_ownership` makes it serve again (`the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live`, `crates/tf_tree/tests/rendezvous.rs`).
>
> **One liveness answer per slot, taken once.** `Snapshot` carries the participant table with `Tree::participant_alive` applied (`F_OFD_GETLK` on the slot's lock byte for a tree from `tf_tree::open`, the `/proc` inference otherwise); both halves read it. **No new id, no new arena field**; severity **warn**. It is detection only: reclamation belongs to the participants through `0028`'s collectors, and a `doctor` check must not mutate a robot's arena. A leaked slot is `1 of 64` spent until every participant stops.
>
> **It skips on `--from-file`:** a freeze copies participant records, so every slot in a `.tft` names an exited process and the check would fire on every correct file. `checks::SlotTable` is the discriminator, a different split from `PushStream` (an ingested bag builds its arena in `doctor`'s own process and has an answerable participant table).
>
> **Amendment — `doctor` opens the lock file (`0028` step 6).** On `--attach`, `doctor` opens the rendezvous lock file, probes every participant byte and reads every identity record; `checks::slot_leak` composes the three facts.
>
> * **`RESERVED` is reported** when its byte is free (byte held is a registrant in flight). `0028` question 6 widened `reclaim`, so the assigner, the owner's hangup callback and `Tree::reap_participants` now collect such a record; the check supplies the name.
> * **The fork case is its own finding**: byte *held*, recorded pid gone (a forked child inherited the descriptors, so the socket never hangs up). It is a different message from a free byte because the responses are opposite: a free byte is for a reaper, and this is a slot no reaper may touch since the kernel's answer is *held*. The remedy is upstream: stop the child or use `spawn`/fork+exec; `0030` closes it at the source.
> * **It is judged from the lock file alone, so the *read-only* inheritor is reported too** (D18 read-only consumers write no arena record and `multiprocessing` forks by default).
> * **Undetected:** a claim whose slot has since been re-granted (the claim word `(epoch << 16) | (slot + 1)` carries the claim's own counter, not the participant's incarnation; closing it is an arena format change, and `FORMAT_VERSION = 3` is not reopened opportunistically).
>
> **The `/proc` half is three-valued** (`recorded_given` in `tf_tree_cli`: running / gone / cannot say, `tf_tree`'s own `alive_given` transposed). Only `ENOENT` on a host that would have shown an entry proves death, tested by reading `/proc/self/stat` (`proc_answers_here`); `EACCES` from `hidepid`, `EMFILE` or an unparseable `stat` line are *cannot say*. Otherwise the fork arm would fire on every healthy slot on a host with no `/proc`.
>
> **A recorded pid is namespace-local** ([`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md)). `recorded_given` answers *cannot say* ahead of the `/proc` classification when the recorded PID namespace differs from the observer's (`Identity` carries it; `0` is *unknown* and keeps the older behaviour) or the observer's `/proc` does not describe its own namespace. The namespace is recorded at registration, never derived at diagnosis (`/proc/<pid>/ns/pid` fails open).
>
> **The finding prints the lock file identity's pid**, since a `RESERVED` record's `pid` is still zero and a read-only slot has no record; where both exist and differ, both are named. Where neither exists the subject is *"slot N, no pid recorded"* and no instruction asks the operator to check a pid. The subject also states `byte free`, `byte still HELD`, or `byte not probed` (a run with no kernel answer: no lock file opened, or an `F_OFD_GETLK` error, which is why `slot_facts` is three-valued). The word-before-byte order (`0028` piece 2) is pinned by signature: `Snapshot::probe_lock_facts` hands a callback the already-captured row and `slot_facts` takes that row, so the hoist does not compile; the argument that it matters is `loom`'s model of `tf_tree`'s `reclamation_verdict`.
>
> **The byte-less `build_shared` creator is still accused:** a served `doctor --attach` run holds a lock file and probes, so a byte-less record reads `(byte free, recorded process unknown)`, which is `SlotLeak::Abandoned`, against a running publisher (`a_byteless_record_in_a_served_arena_is_accused_of_leaking`). `byte unknown` is what `--from-bag` and the in-process fixture take: they keep the predicate the check shipped with, and the message does not claim a probe the run never made.

> **Amendment — `TFT008` is the inter-arrival *spread*, not a p99 against the nominal.** The Detection column read *"p99 inter-arrival ≫ nominal"*; what shipped since Phase 1's `inconsistent-rate` is the **coefficient of variation** of the retained intervals about their own mean, above a threshold. Judging an interval against the declared nominal is what `TFT009` already refuses for gaps (an edge at half its nominal fails a p99 rule on every sample, burying what `TFT007` reports once), and a nominal exists only where a topology file declared one, so keying `TFT008` on it would skip on every arena built without `rate_hz`. A second statistic under the same id is forbidden by the `TFT017`/`TFT018` amendment. The rule is `tf_tree_cli::doctor::check_inconsistent_rates`; the argument is in `checks::tft008`'s doc.

> **Amendment — `TFT007` and `TFT008` carried `TFT009`'s trailing blindness, and they withhold rather than acquiring a second spelling of it.** Every rule in the catalogue measures **between** retained stamps, and a publisher that stopped three weeks ago leaves a full ring of perfectly spaced samples: `TFT007`'s observed rate equals its declaration and `TFT008`'s coefficient of variation is ~0. `doctor --attach` therefore printed `TFT009` calling an edge dead and checks clearing it.
>
> The pair prints only on a **live** source (`checks::live_wall_now` requires `Clock::Wall` *and* `PushStream::RingsUnderWriter`), on an edge whose ring supports an `IntervalShape` (monotone, positive median, at least four intervals) and has been silent for more than `GAP_FACTOR` x that median. `TFT008` is then the second answer on any such arena; `TFT007` only where the topology declared a `rate_hz`, the ring retains more than `RATE_MIN_INTERVALS` intervals and the observed rate is inside `RATE_TOLERANCE`. Held by `checks::tests::a_stopped_publisher_is_not_certified_healthy_by_tft007_and_tft008`.
>
> **They are not given a finding**: a second warn id for one fault inflates `--exit-code warn`. Each **withholds judgement** on such an edge, and where that leaves nothing judged the check **skips** with a reason naming the stopped publisher. `TFT008` also skips when no edge retained enough intervals (`doctor::SPREAD_MIN_INTERVALS`; `checks::tests::tft008_skips_when_nothing_retained_enough_intervals_to_measure`). **One predicate**: `checks::stopped_publishers` serves all three checks and inherits `live_wall_now`, so it answers nothing on a recording or `.tft`. Disclosure is `Meta.notes` via `checks::stopped_publisher_note`. `checks::rate_coverage_note` reads the same map, so a report cannot say both *not run, compared nothing* and *compared 1 of 2* (`checks::tests::the_rate_coverage_note_and_tft007_agree_about_a_stopped_publisher`). `TFT017` is not in this set: a wedged publisher still holds its claim, and *no live writer* is a different question from *no recent sample*.

> **Amendment — `TFT009` was the one check in this group with no skip arm, and it reported `pass` over an empty subject set beside a `TFT008` skip over the same one.** Both of its halves run only over edges `checks::interval_shape` accepted, so where every edge is declined the finding list is empty and rendered as `pass`. The floor is higher than `TFT008`'s (`GAP_MIN_INTERVALS` + 1 = five retained samples). Transient: every arena for its first four pushes per edge, and every publisher restart. Permanent: any edge sized `RingSize::History { rate_hz, secs }` with `rate_hz * secs <= 4`, since `SampleRing::retained` is `capacity - 1`.
>
> The skip reason is three-valued because `interval_shape` declines for three conditions with opposite remedies, returned as a `ShapeGap`: fewer than `GAP_MIN_INTERVALS` intervals (wait or resize), a **negative** interval (read `TFT018`), a non-positive median (a publisher stamping one instant). `checks::gap_evidence_skip` states one clause per class **present**, quoting `checks::GAP_MIN_INTERVALS`. No verdict moves and nothing new fires (`stopped_publishers` reads the same `interval_shape`; a `debug_assert` holds it); `pass` becomes `not run` on sparse arenas, and no exit status changes. Tests: `checks::tests::tft009_skips_when_no_edge_retained_enough_intervals_to_measure_a_gap` and `checks::tests::an_out_of_order_stream_is_not_reported_as_a_dropout`. `checks::silence_coverage_note` reads the check's **outcome** and returns `None` when it skipped, so one report does not explain itself twice.

> **Amendment — `TFT013` has the grace period its row requires, and the evidence is the arena's publishing, not a new field.** The predicate was `kind == Dynamic && head == 0` with no time term, so `doctor` at bringup reported every dynamic edge. There is no declaration timestamp in the arena and none is added (`CLAUDE.md`; [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md)). The grace is measured against **how long the longest-running dynamic publisher has been running**: `(head - 1) x median period`. `EdgeRecord::head` is the monotone total of accepted pushes (invariant 5), so it keeps growing across laps, unlike the retained span, which saturates at `capacity / rate`. Both terms are lower bounds, so the check accuses late rather than early. *Longest* and *dynamic* are both load-bearing (a minimum would let one restarting publisher reset the grace; a static edge's stream must not clear it): `checks::tests::the_grace_period_reads_the_longest_running_dynamic_publisher`. The length is `checks::DECLARATION_GRACE_NS`, `tf_tree_cli`'s choice, like `CLOCK_STEP_MIN_REJECTED_RUN`. An arena in which nothing has published is a separate skip (bringup and total outage are indistinguishable; `TFT017` reports the second).
>
> **The grace evidence is *unobtainable* on some arenas, which is a third skip.** `doctor::median_period` needs **two** retained samples, and an edge with `rate_hz * secs <= 2` retains one for the life of the arena (`history(1.0, 3.0)` rounds to four slots, retains three, and fires). `checks::publish_activity` returns a three-valued `PublishActivity` — `NoPublisher`, `Unmeasurable { .. }`, `Running(ns)` — in one walk. `Unmeasurable` carries the largest ring's retained capacity and the most samples recovered, and the reason branches: a ring that cannot hold two, a large ring given only one (`doctor --attach` at bringup, or a recording with one dated record for an edge), or a publisher stamping one instant; the last two send the reader to `TFT009` and `TFT018`. Whether the grace can be cleared from `(head - 1) / nominal_rate_mhz` where a rate is declared substitutes a declared rate for a measured one and is a §6 amendment. Test: `crates/tf_tree_cli/tests/catalogue.rs::tft013_skips_with_the_ring_size_reason_on_an_arena_whose_publisher_it_cannot_measure` (an integration test, since the claim is reachability) and `checks::tests::tft013_names_which_of_the_three_unmeasurable_arenas_this_is`.

---

## 7. `tf_tree top`

A live read-only participant. TUI first, with an embedded static web view behind `--web`.

TUI panes: topology with per-edge rate/staleness/occupancy and writer identity; a participant list with mode, PID, attach time, and failure counts; a rolling diagnostics feed; and a per-edge detail view with an inter-arrival histogram.

**NORMATIVE constraints on the web view:** a single embedded HTML file plus one JSON endpoint, no build step, no npm, no CDN. Charts in hand-written SVG.

Bind to loopback by default. Serving robot state on `0.0.0.0` by default would be a security bug in someone's deployment.

> **Amendment — `ratatui` is not used.** A `ratatui`/`crossterm` tail inside a workspace with a hard dependency budget would draw four panes of fixed-width text that `crates/tf_tree_cli/src/top.rs` draws in about thirty lines of `ESC[H` / `ESC[K` / `ESC[J`. There is no key handling (raw mode means `termios`, which means `libc` and an `unsafe` boundary `tf_tree_cli` forbids), so the detail view is `--edge <id|name>`, and there is no alternate screen (restoring one on `SIGINT` needs a signal handler). Interactive selection would be a decision record.
>
> * **Ages are against the reference clock `doctor` uses**: `checks::Clock::decide`, a majority vote of per-edge newest stamps against the host clock, falling back to the **median** newest stamp when stamps are in another domain (not the maximum, which hands "now" to the single worst publisher). `top` prints `Clock::label()` in its header. A participant's `attached_at_nanos` is the arena's clock and shows `epoch?` rather than a negative age.
> * **"Observes without perturbing" is a test**: `top::tests::capturing_the_arena_moves_no_counter`.
> * **Frame names and lock-file `comm` are sanitized** by `top::sanitize` before reaching the terminal (the ANSI counterpart of `catalogue::json_escape`); it also keeps `--color never` escape-free.
> * **The participant pane is the arena table ∪ the lock file**, because a read-only participant writes no arena record (D18; §5.6's amendment).
>
> **Rates are observed and never presented as a deviation**: `top` shows a stamp-derived median rate and a head-advance rate side by side and compares neither against a declared one.

> **Amendment — the `--web` half.** **No HTTP crate**: `hyper`/`axum` pull a `tokio` runtime into a workspace with no `async`/runtime, and `tiny_http` is still a dependency for two routes. `--web` is `std::net::TcpListener` with a `std::thread::scope`d thread per connection, no keep-alive, and adds no crate to any manifest; it is not a general-purpose server and must never be pointed at a network (`serve`'s doc).
>
> * **This is the only network socket in the repository**, and §11's "no network" test is scoped to the library's suite (§5.1); `tf_tree`, `tf_tree_core`, `tf_tree_arena` and `tf_tree_ipc` cannot reach this code.
> * **A `Host` guard**: any page the operator visits can `fetch http://127.0.0.1:8787/`, and DNS rebinding makes it same-origin, so a loopback bind refuses any request whose `Host` is not a loopback name, missing `Host` included. `--web 0.0.0.0:8787` gets a stderr warning and no guard.
> * **"No CDN" is enforced by the browser**: every response carries `Content-Security-Policy: default-src 'none'` with `connect-src 'self'`. Two tests scan the page for protocol-relative `src`, `@import`, dynamic `import()` and the four string-to-DOM paths (frame names are arbitrary UTF-8).
> * **A poll arriving inside one interval is answered from the previous document**: one `Sampler` holds all per-tick state and every delta is a difference between two observations, so two tabs would each read half the true rate.
>
> Defects found by the tests: **a peer that connects and says nothing is an outage**, and a per-connection read timeout only sets the slope (five silent sockets cost 10.0 s against 0.008 s), so handling is threaded, capped at `MAX_CONNECTIONS = 64`, with the timeout retiring the socket (`silent_peers_do_not_delay_the_operators_poll`, `a_client_that_never_speaks_does_not_wedge_the_server`, which must hold its silent socket open). The CSP test must assert against the response head only, since `web/index.html`'s comment quotes the header. The non-finite-rate test is a claim about a guard (`IntervalStats::rate_hz` and `observed_hz` return `None` unless positive).

---

## 8. Visualization — deliberately not built

### 8.1 The reasoning

A `tf_tree.rerun` module and a well-known-schema MCAP "viewer channel" both solve a problem that does not exist. A user with a bag already has images, LiDAR, *and* transforms in one MCAP and opens it in Rerun or Foxglove. A transform channel `tf_tree` writes is a re-encoding of the same poses, costing a protobuf dependency, a schema to keep current, a CI job against two viewers, and a support surface.

**`tf_tree`'s value is entirely in things a viewer cannot show**: how fast a query is answered, whether the answer is correct, and what is wrong with the transform tree.

### 8.2 What is genuinely not visible in a viewer — and where it goes

| Information | Not in the bag | Surface |
|---|---|---|
| Clock skew between publishers | ✓ | `doctor` `TFT004` |
| Extrapolation hotspots, with consumer attribution | ✓ | `doctor` `TFT010`, `EdgeCounters` |
| Multi-publisher conflicts | ✓ | `doctor` `TFT001` (Phase 4 bridge) |
| Rate deviation, jitter, gaps | ✓ | `doctor` `TFT007`–`TFT009` |
| Buffer undersizing vs observed lag | ✓ | `doctor` `TFT011` |
| Live per-edge state | ✓ | `tf_tree top` (§7) |

All of it is tabular, time-series and per-publisher, which is what a TUI and a JSON schema are good at.

### 8.3 What survives — and it is not viewer-specific

Two iteration methods, justified independently of any viewer, for export, audit and statistics over stored data:

```python
ds.iter_edge(edge, t0, t1)     # -> (stamp_ns, pose) at STORED sample times, not interpolated
ds.iter_edges(t0, t1)          # -> interleaved across edges, time-ordered
ds.frame_path("lidar")         # -> ["world", "base_link", "lidar"]  root-to-leaf chain
```

**NORMATIVE:** `iter_edge` yields stored samples. Everything else in the API interpolates; "what was actually published" and "what would be interpolated at time t" are different questions.

`frame_path` is a plain frame chain, already needed for error messages and `doctor` output. A Rerun snippet may ship as a documentation example, **never a module**: no optional dependency, no API surface.

### 8.4 If `tf_tree` ever becomes the source of truth

Transforms are genuinely missing from a recording only in a post-Phase-7 deployment where nodes publish to `tf_tree` instead of `/tf`. The **Phase 7 egress bridge** (arena → `/tf`) solves it, making every existing tool work with no viewer-specific code.

### 8.5 One idea explicitly parked

Annotating a copy of a recording with `tf_tree`'s *analysis* (a diagnostics channel on a viewer's timeline) would be additive, but requires rewriting bags and has no requester. **Parked, unbuilt.**

## 9. The benchmark artifact

### 9.1 It is a product, not a script

```
tf_tree bench compare --bag run.mcap --consumers 16 --duration 120s --out report/
```

Runs both stacks on the same data: N `tf2` consumers versus one bridge plus N `tf_tree` consumers. Emits `report/index.html`, `report/results.json` (stable schema, CI-diffable), and the exact environment description needed to reproduce. Ship a container image and a small public sample recording so a stranger can run it in one command.

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

`report::tests::the_required_row_set_is_the_size_of_phase5_section_9_2s_table` counts this table's rows against `crates/tf_tree_bench/src/report.rs`'s `REQUIRED_ROWS.len()`; it counts and does not match names.

The `Plan::at` row is `tf_tree` against itself: the only measurement of the path an **embedder** compiles. `PHASE4.md` §7 gates the C ABI at 5% against in-crate Rust; nothing else gates out-of-crate Rust. [`API.md`](./API.md) §2.3 makes the row and gate normative, with the `#[inline]` attributes and LTO guidance. Report it with the embedder's default profile, **not** this workspace's `lto = "thin"`, which hides the effect.

> **Amendment — there are two embedding measurements, and only the row above is gated.** Both are produced by `just embed-cost` (`crates/tf_tree_bench/src/embed.rs`).
>
> | | measurement | how | status |
> |---|---|---|---|
> | 1 | **the row above** — separate-crate vs in-crate, depth 3 | **one build, one profile**: two identical `#[inline(never)]` bodies, one compiled in `tf_tree_bench`, one in `tf_tree_core` (`bench_probe`, a default-off feature). Read off the `[profile.embedder]` run; `[profile.release]` is the control. | `embedding_cross_crate` in `results.json`, **gated at 5%** |
> | 2 | **profile comparison** — the same out-of-crate body under `[profile.embedder]` against `[profile.release]` | two builds, two processes | **exploratory**, printed by `just embed-cost`, not in `results.json`, not gated — §11.2's shape |
>
> Row 1 is gated on `boundary_ratio`, `out_of_crate_ns` *and* `in_crate_ns`, since a change moving both halves the same way passes a ratio-only gate. Its verdict is `unresolved` when the round-to-round band straddles 5%.
>
> **CORRECTION (2026-09-06) — the criterion is currently unmeasured.** `Plan::at_tagged` sits between `Plan::at` and the fold with no `#[inline]`, so both columns compile to the same out-of-line symbol in `tf_tree_core`: the quotient is 1.0 by construction and `Verdict::Over` is unreachable. `just embed-cost` diagnoses the collapse and refuses unless `EMBED_COST_KNOWN_COLLAPSED=1`. [`API.md`](./API.md) §2.3's 2026-09-06 amendment carries the run and the trade.

### 9.3 Honesty requirements — NORMATIVE

- Identical QoS, identical executor configuration, identical DDS vendor and version, all recorded in the report.
- Both stacks warmed; discard the first N seconds; state N.
- Report `tf2` version, ROS distro, RMW implementation, kernel, CPU model, and THP setting.
- **Report where `tf_tree` is worse**, in the same table and not in a footnote: arena memory floor, attach latency, the operational cost of a format bump, and the bridge as an additional process to supervise.
- Publish the harness source in the same repository. No private benchmark.

If a row cannot be measured fairly, omit it and say why.

**Amendment — "THP setting" is two knobs.** `transparent_hugepage/enabled` governs **anonymous** mappings and `transparent_hugepage/shmem_enabled` governs **shmem**, which is what a live arena is (a sealed `memfd`, `MAP_SHARED`); they disagree by default and on the development host. The report records **both**, as `transparent_hugepage` and `transparent_hugepage_shmem`, each named for its sysfs file (the frozen `.tft` path is a file mapping governed by neither). `REQUIRED_FACTS` requires both, and a test reads each back against its file. `crates/tf_tree_cli/src/hostfacts.rs` exists because `TFT016` made the same mistake.

**Amendment — `Report::validate` enforces bullets 2, 3, 4 and half of 1 and 5.** Bullet 3's six facts and bullet 1's three are a closed list (`REQUIRED_FACTS`), present and non-empty; bullet 2's warm-up must be finite and non-negative, and **positive** whenever an `AbsoluteTiming` or `Ratio` row publishes numbers (`Memory` and `HostIndependent` rows are exempt); bullet 5's mechanical half is that **every** row names a re-deriving command. Two limits are stated: bullet 1's *identical* has one value per key in a one-process report, and bullet 5 is a claim about a repository (`every_command_the_report_names_is_a_command_that_exists` resolves each command against the real `justfile`).

**Amendment — an unavailable row's reason rests on a machine-checked `Ground`.** A guard keyed on phrases is defeated by rewording, so the decisive half of a reason is an enum `Ground` that `Report::validate` re-derives from `cfg!(feature = "tf2")`, `cfg!(all(feature = "shm", target_os = "linux"))`, the row's own `Fitness` verdict, and the measured core count. `MeasuredElsewhere`, `NoInstrument` and `MeasurementRefused` are undecidable there and are separate greppable variants. The three N-way rows (`cpu_per_consumer`, `publish_to_visible`, `scaling_curve`) lead with the permanent gap (this tool is one process and stands up no consumers) and each states a host-independent ground: `MeasuredElsewhere` for the two whose recipe takes the number (`just mp-bench-tf2`, `just tf2-scaling`), `NoInstrument` for `publish_to_visible` (`report::tests::a_host_with_no_obstacle_still_grounds_every_n_way_row`). `Ground` is not emitted into `results.json` (a new key rides a schema bump). `every_command_the_report_names_is_a_command_that_exists` reads both `reproduce:` and `reason:`.

**Amendment — "fairly" is three questions.** `Report::validate` asks each row the question its numbers rest on (`report::Sensitivity`):

| Row reports | Fails on | Survives |
|---|---|---|
| An **absolute duration** (`AbsoluteTiming`) | every check: debug build, SMT, busy machine, governor, unknown core count | — |
| An **interleaved ratio** (`Ratio`) | a debug build, **and a busy machine** | governor, SMT — they land on both arms of a within-round interleave and divide out |
| **Resident memory** (`Memory`) | a debug build, an unreadable `smaps_rollup` | every timing check; Pss is read from `/proc` and involves no clock |
| A **host-independent** figure (`HostIndependent`) | nothing | — |

**Load is not common-mode between these two engines**: `tf2::BufferCore` takes a mutex on every lookup and `tf_tree`'s read path takes none, so a busy host **inflates the quotient in our favour**; hence `busy` reaches the ratio axis. A memory row requires Pss be *readable* (`self_pss_kib` returns `0` when `/proc/self/smaps_rollup` is absent). The **core budget** does not reach a `Memory` row (sixteen workers mapping one `.tft` on four cores share the same pages), which makes **§12 gate 4 measurable on a 4-core host**. A debug build is a different program and reaches everything.

**The `Ratio` axis's first row is the tf2 comparison.** `lookup_ratio_vs_tf2` times a depth-3 hot lookup on both engines in one process, `LerpSlerp` on both sides, arms interleaved within every round with the leading arm alternating, and reports the **median per-round quotient**. On the development host it measured 2.47× (band 2.457–2.532), one draw; [`0025`](./decisions/0025-what-build-the-tf2-ratio-gate-speaks-for.md) records the workspace arm at 1.3–16.7% wide. The tf2 column goes through `tf_tree_tf2_sys` and **flatters `tf_tree`** by the residual FFI boundary, **45.3 ns / 10% at this depth** (`docs/benchmarks/tf2.md`, *Where the 45.3 ns comes from*); `FLOOR` is bounded by the unpaired native-against-native estimate, whose binding-free comparison is `docker/tf2/native_scaling.cpp` (2.7×). `ns_per_lookup` is reported and **never gated**.

**There are two committed baselines**, `results.json` and `results-tf2.json`, checked by `just bench-check` and `just tf2-bench-check`. The status comparison is one-directional, so a single baseline cut with `--features tf2` would fail the default recipe on every host without ROS 2. `baseline::compare` never reads a row's `reason` or `note` (the same blindness that keeps a CPU model out of the gate), so explanatory prose in both baselines can drift (`total_rss_n_consumers`' `reason`, the ratio row's `~21 ns / 8%` note); regenerating with `just bench-baseline-update` would launder a measurement change through a documentation fix. Owed: a check that baseline strings are still producible (asserting a non-zero row count first), or a deliberate regeneration.

The JSON keeps `timing_sensitive` with its original meaning, so `tf_tree.bench-report/2` does not change shape. Each rule carries a test with a verified mutant in `report.rs`.

**Amendment — a one-sided BUDGET with a stated margin may be gated on a host that fails the timing probe (§12 criterion 2).** The `Sensitivity` axes answer *can this host produce this number* for rows compared against a committed baseline, a two-sided question. §12 criterion 2 is a **budget** (*under 10 ms*), and every timing check `Fitness::probe` fails makes a duration **longer**, so on such a host a **PASS with margin is conservative** and a **FAIL is not attributable to the code**. A gate taking this licence owes:

1. **State the margin, per run, from the measurement**: criterion 2 prints its worst reading against the budget and the fitness verdict and reasons beside it.
2. **Gate the arm that is a claim about the code.** The *evicted*-page-cache arm's size dependence is the storage device (the major-fault count does not move between a 2 MiB and a 338 MiB index), so it is reported with the host beside it; the *resident* arm is gated.
3. **A debug build is not refused** (`frozen_open`), since against a budget it is conservative; it prints its profile and says a debug FAIL is not attributable. `just gate2` builds `--release`.

**This is not a new `Sensitivity` variant and not a `bench_report` row**: `tft_open_vs_bag_parse` stays `AbsoluteTiming` and `unavailable` on its missing comparison arm. The budget is held outside the report, in its own binary and recipe (`just gate4`'s shape; gate 4's *"Pss is not a timing measurement"* justification does not transfer). Applying this to a **two-sided** comparison, or to a budget whose margin is inside the host's noise, would be laundering; hence the printed margin.

---

## 10. Open-source readiness

Phase 5 is where the repository becomes publishable, so this is a deliverable, not an afterthought.

- **Name check before anything else.** Confirm `tf_tree` is available on crates.io and PyPI, and decide deliberately whether the proximity to ROS's `tf` / `tf2` package names helps or confuses. Renaming after 1.0 is not an option; renaming now is an afternoon.
- Apache-2.0 / MIT dual (D30), ~~license headers~~, `NOTICE`, SBOM per release. **The header clause is declined** ([`0051`](./decisions/0051-the-licence-travels-with-the-artifact-not-the-file.md)): the licence travels with the artifact and is asserted at all three distribution surfaces; a per-file marker is required by neither licence. The other three are done (§0.0's §10 row).
- `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md` with a real disclosure address.
- **A stated support policy**, honestly scoped: what is supported, what is best-effort, and the response expectation.
- **MSRV policy** and a CI matrix pinning it.
- Documentation site (mdBook): a first-five-minutes path that works — `pip install transform_tree`, three lines, a real result — before any architecture prose. **Both halves are open and they are one question** ([`0052`](./decisions/0052-the-first-five-minutes-nobody-runs.md), `draft`): the three lines exist and run (`README.md`'s *Start with no data at all*, executed by `scripts/quickstart_smoke.py` on every pull request) under the from-source `just quickstart`, and nothing installs the published distribution and imports it.
- CI: the full Phase 1–5 suites on `x86_64` and `aarch64`, ASan/UBSan/TSan, Miri, loom, the nightly `shm_torture`, the benchmark artifact as a regression gate.
- Release automation: `cargo-dist` or equivalent, maturin wheels per Phase 3 §10, PEP 740 attestations, signed tags.

---

## 11. Test plan

- **Three-way bit-identity:** replay one recording into `HeapArena`, `MappedArena`, and `FrozenArena`; identical query set; assert bit-identical `f64` (extends Phase 2 §10). Implemented: `crates/tf_tree_cli/tests/replay_bit_identity.rs::a_replay_into_heap_mapped_and_frozen_arenas_is_bit_identical`, under `just shm-check`. The pairwise tests (`a_replay_into_heap_and_mapped_arenas_is_bit_identical`, `a_frozen_lookup_is_bit_identical_to_the_live_one`) share no input and imply nothing about mapped == frozen; both are kept. Mutation-verified by freezing before the replay.
- **Ingest anomalies:** a synthetic corpus containing every row of §3.2, asserting the whole ingest report JSON byte for byte, with only the temporary source path and crate version interpolated. Implemented: `crates/tf_tree_ingest/tests/anomaly_corpus.rs`. The two hard-error rows (edge-kind change; backward jump past the threshold under `halt`) are driven from the same corpus with one message appended; the below-threshold regression is in the corpus. Red-tested per row. The corpus is written by `tf_tree_ingest::fixture`, so it proves this reader's bookkeeping and not agreement with a real `rosbag2` or DDS writer (the file's header).
- **Out-of-order ingest:** shuffle a recording's messages; the resulting `.tft` must be byte-identical to one built from the ordered source.
- **Spill path:** ingest with `--max-memory` below the dataset size; result identical to the in-memory path. Four tests: grouping (`capped_memory_matches_the_uncapped_path`), one edge spilled and merged in a single pass (`an_oversized_edge_spills_and_matches_the_in_memory_path`), a cap forcing several reduce passes (`a_tiny_cap_reduces_in_several_passes`), and a duplicate `(edge, stamp)` re-merged by a reduce pass (`a_reduce_pass_keeps_the_last_occurrence`). Each asserts the reported peak from **both** sides.
- **Chunk decompression:** conformance against real libzstd (`a_real_libzstd_recording_ingests`, against `testdata/zstd_conformance.mcap`, failing loudly if absent) is distinct from round-trip (`a_zstd_recording_ingests_identically`, `an_lz4_recording_ingests_identically`). Every bomb guard asserts the allocation, not only the error: `a_lying_uncompressed_size_is_refused_before_it_allocates` and `a_high_expansion_ratio_is_refused` check `scratch.capacity() == 0`; the window guard is `a_zstd_frame_demanding_an_oversized_window_is_refused`, bounded below by `the_window_floor_admits_what_a_real_zstd_encoder_declares`. Both length disagreements, each with and without a CRC (`each_codec_round_trips_and_catches_both_length_disagreements`; `uncompressed_crc == 0` means "not computed").
- **Ingest throughput:** time `tf_tree_ingest::run` (both passes) against the recording's stamp span on a generated corpus at the criterion's density, in two `--max-memory` regimes (one fill pass; the group count the four-hour recording forces); gate the grouped one. Each arm asserts its declared pass count, the density is floored, and a gated run refuses a `--floor` below the criterion's. `crates/tf_tree_bench/tests/ingest_throughput.rs` from `just test`; `just gate5` is the gate ([`0050`](./decisions/0050-what-ten-times-real-time-divides.md)).
- **Multi-process page sharing:** 16 processes mapping one `.tft`; total RSS within 1.2× of a single process, from `/proc/*/smaps_rollup` `Pss`.
- **Open time:** two arms. With the page cache **resident**, the open of a gate-scale index must fit 10 ms and agree with the open of a fixture two orders of magnitude smaller. With it **evicted** the number is reported and not gated. Each open is a fresh process and the verdict takes the worst. The evicted arm refuses unless the child's major-fault count witnesses the eviction; a gated run refuses a fixture under 233 MB, two fixtures too close in size, and a `--budget-ms` above the criterion's (the binary's header enumerates). `crates/tf_tree_bench/tests/gate2.rs` from `just shm-check`; `just gate2` is the gate.
- **Fork safety:** a `DataLoader` with `num_workers=16` under all three start methods.
- **Counter contention:** 16 concurrent readers on one edge; no measurable throughput difference against a `counters`-disabled build (§5.7).
- **No network:** the library's test suite under `strace`, asserting `socket(2)` is only ever `AF_UNIX` (§5.1). Scoped to the library's suite; `tf_tree top --web` is an `AF_INET` listener by construction (§7). Implemented: `just no-network` (`scripts/no-network.sh`), in `ci.yml`'s `shm` job on both matrix rows. Its **positive control** is a separate trace over `crates/tf_tree_cli/tests/web.rs` that must find `AF_INET`. It refuses on a missing `strace`, a `strace` that cannot see a socket it is shown, a traced binary that exits non-zero, and a run in which `tests/rendezvous.rs` was not traced or opened no socket.
- **Convenience-path denominator:** `tree.lookup` in a loop credits `lookups_ok` **once per call**, visible before the loop ends (§5.4's withdrawal keeps this). `the_convenience_path_publishes_its_denominator_on_every_call` (`crates/tf_tree/tests/counters.rs`), mutation-verified against one hoisted guard.
- **Diagnostics:** one test per check ID, each with a fixture that triggers exactly that check and no other. **Not met**; §12 criterion 6 names the two ids that cannot meet it. `TFT005` has a unit test pinning `FUTURE_TOLERANCE_NS` at both edges plus an end-to-end one on a wall-clock arena (the fixture stamps from zero, so `Clock::decide` sends it to `NewestStamp`). `TFT016` has a table over synthetic `HostFacts` reading no `/sys` or `/proc` (`checks::tests::every_tft016_arm_fires_and_the_two_corrected_strings_are_pinned`). `rg -n 'fn tft0' crates/tf_tree_cli/src/checks.rs` is the instrument for the rest.
- **`doctor --json`:** schema-validated; adding a check must not break an existing consumer. `catalogue::the_json_report_parses_and_matches_its_documented_schema` runs the real binary and holds the document to the schema block in `catalogue.rs`, which `documented_top_level_keys` parses out, so block, literal and bytes move together (top level only; nested shapes are literals). Asserted: the top-level key set in both directions, the `tf_tree.doctor/1` identifier, every catalogue id once and in id order, and `reason` a string exactly when `status` is `"skipped"`.
- **Web view:** loopback binding asserted; no outbound network requests (assert on the served HTML); the `Host` guard; and an end-to-end test that parses the document a browser receives.
- **`iter_edge` returns stored samples:** push a known irregular sequence, iterate, and assert the exact stamps come back — no resampling, interpolation or reordering.

---

## 12. Gate

1. **Three-way bit-identity passes. MET since 2026-09-08** — `crates/tf_tree_cli/tests/replay_bit_identity.rs::a_replay_into_heap_mapped_and_frozen_arenas_is_bit_identical`, run by `just shm-check` and `ci.yml`'s `shm` job.
2. `.tft` open time under **10 ms** for a 233 MB index (an `mmap` plus header validation; anything more means work is happening that should not). **MET, and gated since 2026-09-05 — `just gate2`.**

   The parenthesis is a statement about *complexity*: every step of `Tree::open_frozen` is O(1) in the index size (§2.4's listing), so the gate is a **regression guard rather than a discovery**.

   | | `just gate2` prints the run's own numbers (`--release`, worst of 8 fresh processes) | |
   |---|---|---|
   | **budget**, resident page cache | more than two orders of magnitude under the 10 ms budget at 338 MiB | **GATED** |
   | **scale invariance**, resident | a small multiple of the 2.1 MiB fixture's open, well inside the 4× bound | **GATED** |
   | evicted page cache | roughly two orders of magnitude slower than resident at 338 MiB | reported |

   No interval is published: a range over a few runs is a sample, and the margin is what the criterion turns on.

   **Only the resident arm gates.** An open takes exactly one major fault when the cache has been dropped and zero when not, at both sizes, and under `strace -T` the `mmap`, `madvise` and header `pread64` are flat across a 2550x range of mapped length; how much the kernel reads to satisfy that fault is a property of the file and mapping. The evicted arm measures the storage stack under the runner. The words are `evicted`/`resident` because `crates/tf_tree_bench/src/bin/attach_bench.rs` uses *cold* for the first attach in a process. See [§9.3](#93-honesty-requirements--normative)'s one-sided-budget amendment.

   **The falsifier is `--prefault`**, which reads every byte of the index inside the timed region (a stand-in for a populate arm reaching the frozen backing, which `populate_edge_rings` refuses by matching on the backing); it puts the open tens of milliseconds over budget and an order of magnitude past the 4× scale bound. A run it cannot evaluate **refuses**: a fixture under 233 MB; two fixtures too close in size; a **gated** run whose eviction did not take (`$TMPDIR` on tmpfs is the usual cause; an ungated run voids the evicted arm and says why); and a gated run against a `--budget-ms` **above** the criterion's. The size floors are disclosed on an ungated run. `crates/tf_tree_bench/tests/gate2.rs` drives all of it through the shipped binary, from `just shm-check`.

   **What it does not cover.** §2.2 says a `.tft` is deliberately not prefaulted, so a gate on the open alone cannot see work *moved* into the first lookup. `tft_open_vs_bag_parse` stays `unavailable`: no artifact holds both halves over one recording.
3. Frozen lookup p50 within **20%** of online. **SUPERSEDED by §2.1, and answered by construction — nothing measures this, and nothing is owed.** §2.1 is **NORMATIVE** that the frozen read path is the **identical** `Plan::at` against a `PROT_READ` mapping: `Tree::open_frozen` hands a `FrozenArena` to the same `&dyn Arena` the heap and `memfd` backings use (`crates/tf_tree/src/frozen.rs:119`, `ArenaBacking` in `crates/tf_tree/src/tree.rs`), which `a_frozen_lookup_is_bit_identical_to_the_live_one` (`crates/tf_tree/tests/frozen.rs:181`) asserts. There is no second implementation to take a ratio between. What differs is residency (a first touch pays a page fault; §2.2), which is gate 4's subject.
4. **16 workers sharing one `.tft`: total Pss within 1.2× of one worker.** **MET — 1.024×, and gated since 2026-09-04 by `just gate4`** (`crates/tf_tree_bench/src/bin/frozen_workers.rs`). 16 workers cost **235.5 MiB** against **229.9 MiB** for one, on a 338 MiB frozen fleet arena (64 robots × 40 s, 1 537 frames, 1 536 edges); solving `total(N) = S + N·p` gives **S = 229.5 MiB shared, p = 0.37 MiB private per worker**. Repeated runs spread over 1.023–1.026×; the verdict has 15% of headroom.

   `just gate4` passes `--gate` and fails the process on a FAIL; `just gate4-python` does not and exits 0. `--gate` refuses `--python` and `--no-touch`, and refuses when there is no N = 1 or N = 16 row. `crates/tf_tree_bench/tests/gate4.rs` drives the shipped binary on a 2-robot fixture that misses `S ≥ 74p` and asserts non-zero with `--gate` and zero without it; `nightly.yml`'s `gate4` job runs the gate and `just shm-check` (`--bins` included, since `required-features = ["shm"]` bins are skipped by `cargo nextest run --workspace`) runs the test.

   > **Correction — until 2026-09-04 this criterion was measured and not gated.** `frozen_workers.rs` printed `PASS` or `FAIL` and returned `Ok(())` on both, so `nightly.yml`'s `gate4` job could not go red ([`0023`](./decisions/0023-the-gate-that-could-not-gate.md)'s shape). Nothing about the criterion, harness or 1.024× moved.

   Three things about the measurement:

   - **The `.tft` has to be large.** `(S + 16p)/(S + p) ≤ 1.2` rearranges to `S ≥ 74p`, so the criterion is about *sharing* only once the arena is hundreds of MiB (gate 2's "233 MB").
   - **Workers must actually read the file.** `--no-touch` measures an unread mapping and comes out at **5.32×, FAIL**. Every worker sweeps every declared edge.
   - **Pss must be sampled while every worker is alive**, behind a barrier: Pss divides a shared page by the processes *currently* mapping it, and collecting as workers finish reported a false **FAIL at 1.43×** (the tell was `p` growing with sweep length). With the barrier `p` is 0.37 MiB at every sweep length.

   > **Amendment — 1.024× is a statement about a *Rust* worker, and must be cited with the worker's language and start method attached.** The criterion is arithmetic about `p` as much as sharing, and `p` is a property of the worker. Measured on the development host (AMD EPYC-Milan, 4 physical / 8 logical cores) with 16 Python workers against the same `.tft`, each sweeping the same stamp grid through `Plan.at_into` and reporting `Pss` behind a two-phase barrier, `just gate4-python` (`crates/tf_tree_bench/python/gate4_worker.py`, under the same `frozen_workers --python` driver as the Rust arm) gives **1.804–1.806×** on CPython 3.14.3 (`S = 244.1–244.6 MiB, p = 13.84–13.86 MiB`; 1.785× by hand on 3.13.12), so `S ≥ 74p` wants ~1 025 MiB where the fixture supplies 338. The no-touch controls fail at 8.86× and 8.120×. On a 39 MiB fixture the start methods separate:
   >
   > | worker | measured `p` | minimum `S` for `S ≥ 74p` |
   > |---|---|---|
   > | Rust (`frozen_workers.rs`, the gate's own) | **0.36 MiB** | 27 MiB |
   > | forked CPython + numpy | **3.36, 3.37 MiB** | 249 MiB |
   > | spawned CPython + numpy | **13.4–14.2 MiB** | ~1 000 MiB |
   >
   > [`0026`](./decisions/0026-the-corpus-shape-of-a-frozen-index.md) reached the same conclusion from a different corpus (Rust 0.37, forked 2.24–2.72, spawned 13.24–13.74 MiB; a 788 MiB corpus fails at **1.248×**); the forked rows disagree and the difference is unattributed. The wedge's audience is the spawned row (§4.3's amendment: `spawn`/`forkserver` is CPython 3.14's default), and a torch worker imports far more than numpy, so 13.44 MiB is a floor. These figures are Pss byte counts and ratios (§9.3's `Memory` sensitivity), worker-bound, not host-bound. Limits: the 3.13 vs 3.14 difference moves `p` 3%; no torch `DataLoader` was in the loop, and raw `os.fork` is `0026`'s open question 2.
   >
   > **This amendment does not change criterion 4 or its MET.** Whether the gate acquires a second worker arm (which start method, what corpus size) is a decision record; gating is a `--gate` flag the caller passes, and the binary refuses `--gate --python` with a message naming this paragraph (`just gate4` exits 1 at 5.380× on a seeded fixture, `just gate4-python` exits 0 at 7.021×). **Wherever 1.024× is cited as evidence for the wedge** (`README.md`, the §9 report, a talk) **it is cited as *a Rust worker sharing a 338 MiB `.tft`***; both recipes name their worker in the verdict line.
5. Ingest throughput ≥ **10× real time** on a representative recording. **MET, and gated since 2026-09-05 — `just gate5`.** [`0050`](./decisions/0050-what-ten-times-real-time-divides.md) answers what the ratio divides, what the density floor is for, why this may be gated on a host that fails the timing probe, and at what pass count the criterion is stated; read it before changing any of the four.

   | | `just gate5` prints the run's own numbers (`--release`, worst of 3 rounds, 50 edges × 100 Hz × 32 s zstd corpus) | |
   |---|---|---|
   | **grouped** — pass two takes the group count this criterion's own recording forces | more than an order of magnitude above the 10× floor | **GATED** |
   | **in-memory** — the default `--max-memory`, one fill pass | also well above the floor, and not what the criterion is stated over | reported |

   The falsifier is a **denser corpus** (measured 2.6× real time at 40× the declared density); `crates/tf_tree_bench/tests/ingest_throughput.rs` drives it per-PR in `just test`. `just bench-check` runs `bench_report` and executes nothing under `benches/`, so an ingest bench there would be run by nothing. `PHASE4.md` §6.3 carries a *different* "10× real time" criterion (ROS 2 bag replay); it is unmet, about the bridge, and unrelated. `just gate5` prints that sentence on every run.
6. Every §6 check has a passing fixture test. **Not met, and it cannot be met by writing tests.** `TFT002` and `TFT003` do not detect in any configuration `doctor` builds (`tf_tree_bridge::StaticStore` is process-local); the criterion stays unmet rather than being re-read as "every check that *can* detect".

   **`TFT002`** has its evidence in `doctor`'s own process on the recording source (`Anomalies::static_conflicts`, printed to stderr and dropped). It needs a route from the ingest report into `checks::Inputs` **and a decision**: the count is a bare `u64` with no edge attached and counts *observations*, so a latched static re-delivered to late joiners inflates it; as an arena-level `error` it would change the exit status of every `--from-bag` CI invocation; per-edge attribution needs a `StaticStore` accessor in another crate. **`TFT003`** is detected on a recording and becomes `IngestError::EdgeKindChanged`, aborting the run, so *"`TFT003` fired"* and *"the recording ingested"* are mutually exclusive. Making it fire means demoting a hard error to a counted anomaly in `tf_tree_ingest` (changing what `tf_tree ingest` and `freeze --from-bag` produce, against §3.2 and §5.7); doing it inside `doctor` would be a second ingest policy. Both are decision records.
7. Benchmark artifact runs from the published container on a clean machine and reproduces the committed `results.json` within tolerance.
8. §10 checklist complete, including the name decision.

Criterion 4 is the wedge's central claim, and criterion 7 is what makes it believable to anyone outside the team.

---

## 13. Definition of done

- [x] `FORMAT_VERSION = 3` shipped in a single commit, with Phase 6 **header fields** reserved and `doctor --explain-version` — `crates/tf_tree_arena/src/header.rs` carries `pub const FORMAT_VERSION: u32 = 3` with the header at 320 bytes, and `crates/tf_tree_arena/src/layout.rs` asserts `layout_hash()` against a literal (`layout::tests::layout_hash_is_deterministic_and_stable`). The region half is retracted (§1.2, [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md)); §0.0's §1 row is authoritative. `explain_format_version` (`crates/tf_tree_cli/src/lib.rs`) is exercised by no test.
- [ ] Publish-side observability derived, not counted — push path unchanged and benchmarked to prove it — **the first conjunct is met and the second is not.** `EdgeCounters` and `ParticipantCounters` (`crates/tf_tree_core/src/counters.rs`) are consumer-side by construction. "Unchanged" is false: [`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md) put a clock-offset sampler on `EdgeWriter::push`, priced by `just push-sampler-cost` as a paired delta in one process at +1.0–1.1 ns (~21–23 %); its reading is registered in [`EVIDENCE.md`](./benchmarks/EVIDENCE.md), it is not a gate, and it is in no workflow. Closing this box means amending its wording against `0036`.
- [x] Nothing in the codebase, CLI, or docs is called "telemetry" — `grep -rIl -i telemetry` (excluding `target/`, `.venv*/`, `.git/`) returns this document alone, in §5.1 and §1.3's rejected sketch. Nothing enforces it (§5.1's amendment).
- [x] `socket(2)` restricted to `AF_UNIX`, asserted in CI — `just no-network`, in `ci.yml`'s `shm` job. Scoped to the library's suite per §11: the five published crates are traced, `tf_tree_cli` is the control, and every other package is traced by nothing (§5.1's amendment).
- [x] Error-path counters always on with no runtime switch; `counters` cargo feature is the only knob — `crates/tf_tree_core/Cargo.toml` and `crates/tf_tree/Cargo.toml` both have `default = ["counters"]`; the regions stay either way (D34). `grep -rn 'var("TF_TREE' crates/tf_tree_core/src crates/tf_tree/src` finds none (§5.3). `crates/tf_tree/tests/counters.rs` (`#![cfg(feature = "unstable")]`, unified in by `cargo nextest run --workspace`) exercises the counters. The counters-*off* build is compiled by `just lint`'s `cargo clippy -p tf_tree_core --no-default-features --features crash-points` and `just stable-tier-check`'s `cargo clippy -p tf_tree --lib --no-default-features`. `pure-hash` is the BLAKE3 backend and unrelated.
- [x] Convenience path keeps its **per-call** `Guard` — **§5.4's NORMATIVE requirement is WITHDRAWN (2026-09-09); read its amendment before reopening this.** Closed by the withdrawal plus the test that pins what it keeps (`the_convenience_path_publishes_its_denominator_on_every_call`). `Tree::lookup_tagged` builds a `Guard` per call (`crates/tf_tree/src/tree.rs`); the per-thread structure is the plan cache (`crates/tf_tree/src/cache.rs`). [`0022`](./decisions/0022-the-per-call-guard-and-the-unwatched-gate.md) leaves the per-call guard deliberately, answering the cost with `tft_plan_at_many`. §0.0's §5 row reads **Done** because the `Cell<u32>` accumulation flushed on `Drop` is wired.
- [x] `freeze --from-live` captures counters into the manifest — **met, and the wording is superseded by §5.6's amendment: they land in the arena image, not the manifest.** The test is `freezing_carries_the_counter_regions` (`crates/tf_tree/tests/frozen.rs`), `#[cfg(feature = "unstable")]` in a target with `required-features = ["shm"]`, run by `just shm-check`'s `cargo nextest run -p tf_tree --features shm,unstable --test frozen`.
- [x] `.tft` format implemented, 2 MiB-aligned arena, CBOR manifest, `source_digest` — `crates/tf_tree_arena/src/frozen.rs` carries `ARENA_FILE_ALIGN = 2 * 1024 * 1024`, the manifest offset/length pair and `source_digest: [u8; 32]`; `crates/tf_tree/src/cbor.rs` is the manifest's **writer** (a definite-length RFC 8949 subset) and there is deliberately **no decoder**. Both modules are `#[cfg(all(feature = "shm", target_os = "linux"))]`, run by `just shm-check`.
- [ ] Three-way bit-identity test green in CI — the property was composed from three pairwise tests, one of which (`a_frozen_bag_answers_like_the_tree_it_came_from`, `crates/tf_tree_ingest/tests/frozen_bag.rs`) compares `Result<Iso3, LookupError>` by value and not `to_bits`. §12 criterion 1's single test now drives one recording into all three backings under `just shm-check`.
- [x] Offline Python API is the *same* API; no parallel surface introduced — `tf_tree.open_file()` returns the ordinary `Tree` (`python/tf_tree/_core.pyi`), and `tf_tree.ingest_bag(path)` does too ([`0046`](./decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md)); no `freeze_bag` (§0.0's §4 row). Gated by `just py-test` (`.venv`, 3.14) and `just py-test-freethreaded` (`.venv-t`, 3.14t), two steps of `ci.yml`'s `python bindings (pytest, 3.14 + 3.14t)` job.
- [x] Ingest report emitted as JSON and human summary; every §3.2 anomaly covered by a fixture — `crates/tf_tree_ingest/src/report.rs` carries `to_json` (schema-tagged) and `summary`. Each §3.2 row has a test in `crates/tf_tree_ingest/tests/ingest.rs`: `duplicates_resolve_last_wins`, `zero_stamps_are_dropped_and_counted`, `future_stamps_are_kept_and_reported`, `edge_kind_change_is_a_hard_error`, `clock_reset_halts_but_jitter_does_not`, `static_conflicts_are_reported_and_first_wins`, `an_edge_whose_every_sample_was_dropped_is_declared_and_flagged`; the one-corpus, whole-document test is §11's `anomaly_corpus.rs`.
- [x] All 16 diagnostic checks implemented with stable IDs, `--json`, and `--exit-code` — the number is stale downward by design: `TFT001`–`TFT019` ship, ids appended and never renumbered (`grep -o 'Tft0[0-9]*' crates/tf_tree_cli/src/catalogue.rs | sort -u`). §0.0's §6 row is authoritative on how many *detect*. Tests: `catalogue::tests::identifiers_are_unique_and_round_trip`, `every_id_is_reported_and_every_skip_states_a_reason`, `the_json_summary_agrees_with_the_exit_status`, `the_exit_code_gate_has_a_warn_tier_and_an_unchanged_default`.
- [x] `tf_tree top` TUI plus embedded web view with no build step, loopback-bound
- [ ] `iter_edge` / `iter_edges` / `frame_path` present on both live and frozen arenas, with `iter_edge` yielding stored samples — **none of the three exists in any language** (`grep -rn 'iter_edge\|iter_edges\|frame_path' crates/ python/` returns nothing). Blocked on a record: [`0026`](./decisions/0026-the-corpus-shape-of-a-frozen-index.md) step 3 (per-episode versus per-corpus); its steps 6 and 7 landed as [`0027`](./decisions/0027-the-48-byte-frame-name-store.md), and its step 2, `freeze_from_arrays`, is the other half §0.0's §3 row records as absent. Before writing a signature: `API.md` §1 R1 and §7, and `CLAUDE.md`'s rule against a second spelling.
- [x] No viewer dependency, channel, schema, or plugin anywhere in the repository — §8's finished state. `grep -rn 'foxglove\|rerun\|rviz\|plotjuggler' --include=Cargo.toml .` returns nothing; nothing enforces it.
- [ ] Benchmark artifact reproducible from a published container by someone outside the team — **the artifact exists and the container does not.** `just bench-report` / `bench-report-shm` emit `report/{results.json,index.html}`, `crates/tf_tree_bench/baseline/results.json` is committed, and `just bench-check` gates it in `ci.yml`'s `bench-gate` job on a fresh runner. `docker/` holds only `tf2`, the ROS 2 differential build environment, published nowhere. A container must choose its build: `Tree::open_frozen` is `shm`-gated and the two `.tft` rows are `UNAVAILABLE` without it; §12 criteria 2 and 4 (`just gate2`, `just gate4`) need a `--release --features shm` build, while `just gate5` needs no `shm`.
- [x] "Where we are worse" section present in the benchmark report — `crates/tf_tree_bench/src/report.rs` carries §9.3's topics verbatim; tests `report::tests::the_where_we_are_worse_entries_are_required_and_must_state_the_cost` and `report::tests::a_worse_entry_with_no_numbers_must_say_why`; regenerated by `just bench-check`.
- [ ] §10 open-source checklist complete, name decision made and recorded — **the name decision is closed and the checklist is not.** [`0008`](./decisions/0008-the-name-tf-tree.md) records it with the PyPI refusal (distribution `transform_tree`, module `tf_tree`). `license headers` is **declined** ([`0051`](./decisions/0051-the-licence-travels-with-the-artifact-not-the-file.md)). Open: the `pip install transform_tree` first-five-minutes path ([`0052`](./decisions/0052-the-first-five-minutes-nobody-runs.md)); the **mdBook site** (no `book.toml`, `SUMMARY.md`, recipe or workflow); and a **signing key** (`release.yml` warns, and refuses when `REQUIRE_SIGNED_TAGS` is true; every tag through `v0.0.5` is unsigned). `CONTRIBUTING.md`'s *Releasing* section carries the one-time setup.
- [x] §12 gate met, or a written explanation of which criterion failed and by how much — **the tick records that the explanation exists, not that the gate is met.** Four states: met, failed by a stated margin, **held by nobody**, and **superseded** (not a failure).

  | criterion | verdict | evidence |
  |---|---|---|
  | 1. three-way bit-identity passes | **met since 2026-09-08** | `a_replay_into_heap_mapped_and_frozen_arenas_is_bit_identical` |
  | 2. `.tft` open under 10 ms for a 233 MB index | **met, and gated since 2026-09-05** | `just gate2`; the report row stays `UNAVAILABLE` on its missing comparison |
  | 3. frozen lookup p50 within 20% of online | **superseded, and answered by construction** | §2.1: the identical `Plan::at`; criterion 3's own paragraph |
  | 4. 16 workers sharing one `.tft`, total Pss within 1.2× of one worker | **met at 1.024×, and gated since 2026-09-04** | `just gate4` (nightly); `crates/tf_tree_bench/tests/gate4.rs` (per-PR, under `just shm-check`); `just gate4-python` **reports** and exits 0 by decision |
  | 5. ingest throughput ≥ 10× real time | **met, and gated since 2026-09-05** | `just gate5`; [`0050`](./decisions/0050-what-ten-times-real-time-divides.md) |
  | 6. every §6 check has a passing fixture test | **partly met, and some ids cannot meet it in any configuration** | `TFT002`/`TFT003` are unconditional skips (§0.0's §6 row). `crates/tf_tree_cli/tests/catalogue.rs` proves a correct populated live tree stays quiet; the unit tests in `crates/tf_tree_cli/src/checks.rs` prove each check fires (`rg -n 'tft0[0-9][0-9]' crates/tf_tree_cli/src/checks.rs`, read against the `mod tests` boundary; no set is stated here) |
  | 7. benchmark artifact runs from the published container and reproduces `results.json` | **held by nobody — there is no published container** | box 16 above; the clean-machine half runs in `bench-gate` |
  | 8. §10 checklist complete, including the name decision | **partly met — the mdBook site, a signing key, and the `pip install` first-five-minutes path; the header clause is declined** | box 18 above |

- [ ] `docs/PHASE6.md` written, carrying forward the reserved **header fields** and the Phase 4 surprise log — no such file. The **region table is not reserved**, so a Phase 6 region is a second `FORMAT_VERSION` break, **scheduled rather than owed** ([`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md); its queue is [`PROJECT.md`](./PROJECT.md) §5.1); `CLAUDE.md` carries the standing rule against opportunistic arena fields. The surprise-log half waits on `PHASE4.md` §1, which is not satisfiable by code. Whatever it says must reconcile with [`0009`](./decisions/0009-descoping-phase-6.md).
