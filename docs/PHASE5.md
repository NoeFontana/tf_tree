# tf_tree — Phase 5 Implementation Specification: Offline, Observability, and the Adoption Wedge

> **Companions:** `docs/PROJECT.md` (decision log), `docs/PHASE1.md`–`PHASE4.md`.

**Deliverable:** the artifacts that make `tf_tree` useful to people who have adopted nothing.

Per D28, every user of this phase changes nothing about their robot. They point a tool at a bag, or attach a read-only process to a running system, and get something `tf2` cannot give them. That is the wedge, and it is also what produces the evidence gating Phase 7.

**Framing.** Phases 1–4 built an engine. This phase builds the two products that engine makes possible: a **frozen transform index** that turns a multi-gigabyte bag into a memory-mapped file queryable at native speed from sixteen dataloader workers at once, and an **observability layer** that surfaces a catalogue of transform pathologies which are currently invisible. Neither requires anyone to migrate.

---

## 0.0 Implementation status

**Not started.** This section is the live status table, in the style of
`PHASE2.md` §0.0, and is updated as work lands.

| Area | Status |
|---|---|
| §1 `FORMAT_VERSION = 3`, Phase 6 regions reserved | Not implemented |
| §2 Frozen arena (`.tft`) | Not implemented |
| §3 Bag ingestion | Not implemented |
| §4 Offline Python API | Not implemented |
| §5 Diagnostic counters | Not implemented |
| §6 Diagnostics catalogue `TFT001`–`TFT016` | Not implemented |
| §7 `tf_tree top` | Not implemented |
| §8 Visualization | **Deliberately not built** — this is the finished state, not a gap |
| §9 Benchmark artifact | Not implemented |
| §10 Open-source readiness | Not implemented |

### What this development environment can and cannot gate

- **No ROS 2 installation.** This constrains §3 less than it looks: MCAP is a
  self-describing container with a Rust reader that has no ROS dependency, and
  `tf2_msgs/msg/TFMessage` is decoded from the schema in the file. A `.mcap`
  fixture can therefore be written and read here with no ROS present. What
  cannot be gated is ingestion of a **real** recording produced by a real DDS
  stack, and rosbag2's sqlite3 backend against a real rosbag2 writer.
- **No GPU**, so anything touching device memory stays untested.
- **CPython 3.12.3 only**, no free-threaded build, so §4's additions inherit
  Phase 3's existing coverage gap on 3.14t.
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
| Diagnostics catalogue | 14 checks, each with a detection rule and a severity (§6) |
| `tf_tree top` | TUI plus an embedded static web view (§7) |
| Stored-sample iteration | `iter_edge` / `iter_edges` / `frame_path` — audit and export, viewer-neutral (§8.3) |
| Benchmark artifact | one command, reproducible, honest, CI-gated (§9) |
| Open-source readiness | the checklist that has to be true before publishing (§10) |

### Out of scope — NORMATIVE

| Excluded | Why |
|---|---|
| `tf2_ros::Buffer` shim | Phase 7, gated on this phase's evidence |
| Arena → `/tf` egress | Phase 7 |
| Covariance, splines, CoW branches | Phase 6 — but §1 reserves their layout space now |
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
| `covariance_region_off: u32`, `covariance_stride: u32` | **6** | header, zero when absent |
| `spline_region_off: u32`, `spline_degree: u8` | **6** | header, zero when absent |
| `frame_kind: u8` (link / sensor / map / virtual) | 5 | `FrameRecord` reserved |
| Header `_reserved` | — | keep ≥ 64 bytes after all of the above |

Regions whose Phase 6 content does not exist yet are declared in the header with offset `0`, meaning absent. Phase 6 then fills them **without another layout change**, because the region table already accounts for them.

**NORMATIVE:** recompute `layout_hash` and bump `FORMAT_VERSION` to 3 in one commit. Ship a `tf_tree doctor --explain-version` that prints both versions and the required action when a mismatch is detected, since this is the error operators will meet during the upgrade.

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

The manifest is CBOR rather than a packed struct because it is cold, variable-length, and worth being able to inspect with a generic tool. Everything hot is in the arena.

`source_digest` makes a `.tft` traceable to the recording it came from, which matters the first time a training result cannot be reproduced.

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

### 3.3 Sources

- **MCAP** — primary. Read `tf2_msgs/msg/TFMessage` via the schema, not by assuming a topic name; support `/tf`, `/tf_static`, and remapped equivalents.
- **rosbag2 sqlite3** — supported, lower priority; convert to MCAP where practical.
- **A running arena** — `tf_tree freeze --from-live --duration 60s` snapshots a live system. Useful for capturing a fault in the field.
- **Python** — `tf_tree.freeze_from_arrays(...)` for users whose poses are not in a bag at all. This is how a non-ROS user gets in the door, and it should exist from day one.

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

---

## 5. Diagnostic counters

### 5.1 "Telemetry" is the wrong word — NORMATIVE

Rename it everywhere: **counters**, or **diagnostic counters**. Never "telemetry", in the code, the docs, the CLI, or the changelog.

In 2026 "telemetry" means the software phones home. A robotics team evaluating a library whose documentation says "telemetry" will assume network egress, and some will block it at procurement without reading further. Nothing here leaves the machine — these are counters in shared memory on the same host — and the name should say so.

Pair the rename with a stronger, testable claim:

**`tf_tree` opens no network sockets. Ever.** The only socket in the entire library is the Phase 2 `AF_UNIX` rendezvous socket, which is a filesystem path on the local host. Phase 8 replication will add network transport, and when it does it will be an explicitly enabled, separately named component.

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

### 5.6 Counters are captured in snapshots

Arena counters die with the arena. On a robot, the interesting question is usually "what did the counters say just before this went wrong", so:

**NORMATIVE:** `tf_tree freeze --from-live` copies the counter regions into the `.tft` manifest, and `doctor --json` output is timestamped and appendable. A field snapshot then carries the diagnosis with it, rather than requiring the fault to be reproduced on a bench.

### 5.7 What must still be measured

Publish the cost of the non-atomic `Guard` increment, and confirm under sixteen concurrent readers that the flush-on-drop pattern shows no measurable contention. If it somehow does, shard by participant slot (`counters[edge][slot & 7]`) and sum on read — but measure before adding that complexity.

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

Output modes: human (default, coloured, grouped by severity), `--json` (stable schema, for CI), and `--exit-code` (non-zero if any error-severity check fires) so `doctor` can gate a robot's startup or a CI job.

**`TFT004` deserves special care** — it is the check most likely to find something nobody knew. Compute per-publisher offset between header stamp and arena receipt time, track a rolling median, and report publishers whose median differs from the fleet median by more than a threshold. On a multi-machine robot with imperfect PTP this finds real problems that present as intermittent extrapolation errors.

---

## 7. `tf_tree top`

A live read-only participant. TUI first (`ratatui`), with an embedded static web view behind `--web`.

TUI panes: topology with per-edge rate/staleness/occupancy and writer identity; a participant list with mode, PID, attach time, and failure counts; a rolling diagnostics feed; and a per-edge detail view with an inter-arrival histogram.

**NORMATIVE constraints on the web view:** a single embedded HTML file plus one JSON endpoint, no build step, no npm, no CDN. Charts in hand-written SVG. The moment this needs a frontend toolchain it becomes a maintenance liability that outlives its usefulness, and a small-team infrastructure project cannot afford that.

Bind to loopback by default. Serving robot state on `0.0.0.0` by default would be a security bug in someone's deployment.

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
| Extrapolation hotspots, with consumer attribution | ✓ | `doctor` `TFT010`, telemetry |
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
- **Spill path:** ingest with `--max-memory` set below the dataset size; result identical to the in-memory path.
- **Multi-process page sharing:** 16 processes mapping one `.tft`; assert total RSS is within 1.2× of a single process, measured from `/proc/*/smaps_rollup` `Pss`.
- **Fork safety:** a `DataLoader` with `num_workers=16` under all three start methods.
- **Counter contention:** 16 concurrent readers on one edge, each holding a long-lived `Guard`; assert no measurable throughput difference against a `counters`-disabled build.
- **No network:** full suite under `strace`/seccomp, asserting `socket(2)` is only ever `AF_UNIX` (§5.1).
- **Convenience-path guard reuse:** assert `tree.lookup` in a loop performs O(1) atomic flushes, not O(n).
- **Diagnostics:** one test per check ID, each with a fixture that triggers exactly that check and no other.
- **`doctor --json`:** schema-validated; adding a check must not break an existing consumer.
- **Web view:** loopback binding asserted; no outbound network requests (assert on the served HTML).
- **`iter_edge` returns stored samples:** push a known irregular sequence, iterate, and assert the exact stamps come back — no resampling, no interpolation, no reordering.

---

## 12. Gate

1. **Three-way bit-identity passes.**
2. `.tft` open time under **10 ms** for a 233 MB index (it is an `mmap` plus header validation; anything more means work is happening that should not).
3. Frozen lookup p50 within **20%** of online (accounting for the deeper binary search).
4. **16 workers sharing one `.tft`: total Pss within 1.2× of one worker.**
5. Ingest throughput ≥ **10× real time** on a representative recording.
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
- [ ] `tf_tree top` TUI plus embedded web view with no build step, loopback-bound
- [ ] `iter_edge` / `iter_edges` / `frame_path` present on both live and frozen arenas, with `iter_edge` yielding stored samples
- [ ] No viewer dependency, channel, schema, or plugin anywhere in the repository
- [ ] Benchmark artifact reproducible from a published container by someone outside the team
- [ ] "Where we are worse" section present in the benchmark report
- [ ] §10 open-source checklist complete, name decision made and recorded
- [ ] §12 gate met, or a written explanation of which criterion failed and by how much
- [ ] `docs/PHASE6.md` written, carrying forward the reserved regions and the Phase 4 surprise log
