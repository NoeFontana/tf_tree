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
| §3 Bag ingestion | **Partly done — MCAP only.** `tf_tree_ingest` is a new workspace member; §3's opening note rules out `tf_tree_core`/`tf_tree_arena`, and it is not in `tf_tree_cli` because §4's offline Python API needs the same logic and cannot depend on a binary crate. **That consumer exists as of [`0046`](./decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md)** — `tf_tree.ingest_bag` — and until it did, the crate boundary had been drawn and paid for for a caller with no dependency edge to it (`grep -c tf_tree_ingest crates/tf_tree_py/Cargo.toml` was 0). `digest_file` moved to the crate root and its `blake3` dependency stopped being optional so that the binding can report a recording's digest on platforms with no frozen backend; that adds nothing to any build, because `tf_tree_core` already requires blake3. §3.1's two passes **including the spill-to-run-file**, §3.3's MCAP source (schema-based discovery, so remapped topics are found) and every §3.2 row are implemented and gated by `cargo nextest run --workspace`. `tf_tree ingest --bag` needs no features; `tf_tree freeze --from-bag` needs `shm` (the frozen backend does) and is gated by `just shm-check`. **Of the four things that were not done, two are closed and two are now decisions with arguments rather than gaps:** §3.1's run file is built — grouping still handles every recording whose largest single edge fits the cap, and one edge over the cap on its own is spilled, *reduced* in bounded passes and merged, so `IngestError::EdgeExceedsMemoryCap` is gone (see §3.1's amendment, including why the reduce pass is not optional); `--on-clock-reset=split` **stays refused with the argument recorded in §3.2's amendment** — it would turn one ingest into N arenas and change every downstream contract, to do worse what cutting the recording already does; §3.3's rosbag2-sqlite3 source **stays absent on a measured dependency finding** (§3.3's amendment: `rusqlite` vendors C, `prsqlite` has no licence and `cargo deny` refuses it, `sqlite-rs`/`sq3_parser` are header parsers) but a `.db3` is now *diagnosed* as one, with the `ros2 bag convert` remedy, instead of being reported as a corrupt MCAP. **`freeze_from_arrays` is still absent**, and it is the one of the four that is a schedule rather than a decision: it needs no dependency and no format change, only a `tf_tree_py` entry point that builds a `Tree` from NumPy arrays, and its gate is `just py-test` / `just py-lint` on two interpreters rather than `cargo nextest`. `Tree.freeze()` — the way *out* — already exists (§4 row). **`--max-memory` bounds pass two's sort buffers and *not* the arena**, which is the larger of the two at a measured 78 B/sample against the buffers' 64 — the arena is the output and cannot be capped. `ingest::fill` carries the table and `tests/memory.rs` asserts it; an earlier revision claimed "peak memory is the cap either way", which was false. **§0.0's `default-features = false` on `mcap` costs a measured ~1.8× per pass on a compressed recording and nothing else, and §0.0 carries the amendment:** chunks are taken whole (`emit_chunks`) and decoded by pure-Rust `ruzstd`/`lz4_flex` behind a default-on `compression` feature, so a zstd or lz4 recording — what rosbag2 and Foxglove write by default — ingests transparently with no C build step and `PHASE2.md` §2 untouched. `CompressedChunk` now means a codec outside the specification or a `--no-default-features` build, which is what the `mcap compress --compression none` remedy is for. **The price is measured, not asserted:** `survey` over a 160 000-transform recording takes 0.027 s uncompressed, 0.035 s for lz4 and 0.048 s for zstd, and `ruzstd` decodes libzstd frames at about a quarter of libzstd's own rate — multiplied by a pass count that is `1 + groups + spilled edges`, not a flat two. §12's throughput gate is still met by a wide margin, and an earlier revision of this row said the rule "cost nothing", which was the same kind of overclaim as the "peak memory is the cap either way" sentence three clauses up. Decompression is bounded **three** times before anything is allocated: `uncompressed_size` absolutely (64 MiB) and as an expansion ratio (1024×), because neither codec crate bounds total output — and the zstd decoder's *window*, which is a separate field in the codec's own header that neither of those can see. That third bound closed a measured defect: a 26-byte payload of two concatenated zstd frames, with an honest `uncompressed_size` and a correct CRC, decoded to the right answer and drove the allocator to a 134 MiB peak, unaffected by either knob. A truncated *compressed* chunk stays unrecoverable and is reported as truncation, not corruption; zstd is checked against a committed real-libzstd fixture and lz4 against a frame hand-authored from its specification (one frame, not a whole recording — §12 states the remaining asymmetry), and `just ingest-check` — now also run by CI's `test` job — compiles and tests the codec-free configuration that `--workspace` cannot see, plus asserts against the dependency graph that the shipped CLI still links both codecs, which no test inside that crate can. **A truncated recording is read up to the cut** and reported as truncated rather than refused — a SIGKILLed recorder is how bags in the field end. **A review of the spill path found two real defects and they are fixed:** the temporary file's name was derived from the *edge slot*, so two concurrent `fill` calls in one process picked the same path and `truncate(true)` let the second empty the first's inode — silently interleaved samples, no error, and a deterministic collision rather than a race wherever the unlink-at-create cannot run; it is now a process-wide `AtomicU64`. And **the reduce pass's cross-run tie order was asserted by nothing** — two order-inverting edits left the whole workspace green while resolving a duplicate to the wrong pose — so `a_reduce_pass_keeps_the_last_occurrence` now gates it. Two accounting corrections came with them: a reduce pass holds **two** staging buffers and counted one, and the run index (16 B per run) is real memory the cap does not bound and is now reported as `peak_run_index_bytes` rather than omitted. **What is still not gated, and is said rather than papered over:** the *per-run* sort's stability. Swapping it for `sort_unstable_by_key` survives the whole suite, because a run at the caps these tests use is short enough that `sort_unstable` insertion-sorts and is stable in fact; the in-memory sort it mirrors *is* gated, by the two tests that compare the paths. **Five amendments below**: declaration order is canonical, the reset threshold is not the bridge's question, the reset *guard* is per edge, the cap is enforced by two mechanisms and not one, and `split` is a decision. |
| §4 Offline Python API | **Done, including all three of §4.4's deltas, and since 2026-08-29 the way *in* from a recording** — `tf_tree.ingest_bag(path)` returns an ordinary `Tree`, and `Tree.source` carries the recording's path and BLAKE3 digest so that `ingest_bag(p).freeze(out)` writes a `.tft` traceable to `p` ([`0046`](./decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md)). **There is no `freeze_bag` beside it and the first draft had one**: `tf_tree_ingest::tft::freeze_bag` is `digest_file` + `run` + `freeze_to`, it streams the *digest* and not the tree, and `Tree::freeze_to` already took `source_digest` — so a second entry point would have been a second spelling of the composition, differing only in whether provenance got filled in, and differing *silently*. `Tree.source` is **dropped by `publisher()`**, because a tree that can be written to may hold samples the recording does not and a wrong digest defeats the one question §2.3 gives the field. Gated by `tests/python/test_ingest.py` against the committed conformance recording, whose invalidation test is a control-and-mutant pair one `publisher()` call apart**, with §4.2 trimmed and §4.3's *reason* corrected — see the two amendments in those sections. `tf_tree.open_file()` returns the ordinary `Tree`, so §4.1's "no parallel offline API" is structural rather than promised; `Tree.freeze()` is the Python way *out*, which is also what makes §4.1's claim testable from Python at all (`tests/python/test_frozen.py` compares live against frozen bit-for-bit through `plan.at`). Of §4.2's five helpers only `span` is API: `resample` is one line of NumPy over `at`, and `edges`/`gaps`/`manifest` need §3's counting pass and a CBOR reader, neither of which exists. **Gated by `just py-test` (CPython 3.14, 200 passed, 2 skipped) and `just py-test-freethreaded` (3.14t, 202 passed — the two skips are the free-threading tests, which only that interpreter can run) — so §4 does *not* inherit Phase 3's 3.14t gap; `uv` fetches the free-threaded build even though the host interpreter is 3.12.3.** **Of §4.4's three API-contract deltas, item 2 — introspection, `tree.frames()` / `tree.edges()` / `plan.edges()` — has landed** (`API.md` §6 row 8), as the *names* half only, which is what §4.4 authorises; the enumeration lives in `tf_tree_py` rather than on `Tree` and the follow-up to consolidate it with `tf_tree_c::unstable`'s and `tf_tree_cli`'s copies is filed in `frames_impl`'s doc comment. **All three refuse a tree inherited across a `fork()`** rather than describing the poison arena `Tree::view` substitutes, and `Tree.instance_uuid` was brought to the same rule — it returned the all-zero value that elsewhere means "in-process". `Tree.__repr__` is the deliberate exception, because a repr that raises breaks the debugger pane a fork victim is reading; it prints `detached-by-fork`. **Items 1 (`Layout::QuatTwist`) and 3 (`from_parts`/`from_timespec`/`from_ros`) are now done too** — `API.md` §6 rows 7 and 9. Item 1 is a keyword-only `layout=` on `at`/`at_into` serving all four layouts, plus an `interp=` on `build`/`open(create=...)`, whose default moved from `"lerpslerp"` to the engine's own `"sclerp"` (`API.md` §3) so that a Python-built tree answers a twist without one; item 3 is `tf_tree.from_parts` and `tf_tree.from_ros` on the Python side and `tft_stamp_from_parts`/`tft_stamp_from_timespec` on the C side (ABI minor 3 → 4), with **one refusal table asserted on both sides of the boundary** — the successes were never the risk. |
| §5 Diagnostic counters | **Done**, §5.6 included — see its amendment: the capture is structural, not a step. Structs and regions landed with §1; §5.4's `Guard` accumulation, the error-path increments and §5.5's default-on `counters` feature are wired. §5.7's measurement is `cargo run --release -p tf_tree_bench --bin counter_cost`: **no measurable contention at or below the CPU count**, so the sharding fallback is not justified. |
| §6 Diagnostics catalogue `TFT001`–`TFT019` | **Partly done.** All nineteen ids exist and are reported (§6's second amendment appends `TFT017` *unclaimed dynamic edge* and `TFT018` *out-of-order stamps*, so **nothing is reported id-less any more**, and its third appends `TFT019` *a wall clock stepped backwards*, which attributes `TFT018`'s evidence rather than detecting anything of its own; the ids are appended and never renumbered, which is what keeps `--suppress` and `--json` compatible); `--json` (schema `tf_tree.doctor/1`), `--exit-code` and `--suppress` are wired. **Seventeen detect** — `TFT001`, `TFT004`–`TFT019` — of which **thirteen run on the reference fixture**: `tf_tree doctor` reports `11 passed, 2 fired, 6 not run` of nineteen — `TFT016` moved from passed to fired when it started reading `transparent_hugepage/shmem_enabled`, the file that governs the live arena's `memfd`, in addition to `transparent_hugepage/enabled`, which does not. Reading only the latter reported this host as healthy while `MappedArena`'s `MADV_HUGEPAGE` was a silent no-op; §2.3's amendment carries the measurement. **Two cannot detect anything in any configuration and say so** rather than passing: `TFT002`/`TFT003`, owned by `tf_tree_bridge::StaticStore`, whose state is process-local. **`TFT004` left that group on 2026-08-27** ([`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md) steps 1–4): `ClaimRecord::clock_offset_nanos` records `wall clock - stamp` per publisher, and the check reads it. **What it detects is narrower than §6's opening asks for, and the amendment there says why** — an offset is clock error *plus* stamp-to-push latency, and one sample cannot separate them, so it fires only past a bound no publish pipeline could account for and reports the fleet spread as a note. The fleet-relative rule needs drift over time, which `tf_tree top` polls for and `doctor` does not have. **Ten more skip conditionally**, on evidence rather than on capability: **`TFT004` (four ways — a replayed source, an arena at rest, `TFT005`'s epoch condition, and nothing sampled yet)**, `TFT001`, `TFT018` and `TFT019` (**the push stream, not the arena's liveness** — `TFT001` needs a per-sample writer, which no arena and no recording carries; `TFT018`/`TFT019` need an arrival invariant 6 *rejected*, which only the fixture and a recording carry, since a ring holds none), `TFT005` (the arena's stamps do not share an epoch with the system clock), **`TFT007` (nothing in *this* arena was comparable — either no edge declares a nominal rate, or the declaring edges have not retained enough intervals to measure one; the skip reason says which)**, **`TFT010` and `TFT011` (the §5 counters carry no verdict — either the engine was built without `counters`, or *this arena has served no lookups*, which is the amendment below)**, **`TFT014` (a frozen `.tft` is a byte copy of the arena, participant records included, so every slot in it names a process that exited when the freeze finished and a file has no assigner for a leaked slot to wedge — see §6's `TFT014` amendment)** and `TFT016` (non-Linux host). **`TFT007` was in the first group and is now in the second** — the amendment in §6 records how: a topology file's `rate_hz` is carried into `EdgeRecord::nominal_rate_mhz`, with no arena field added and no format bump. The reference fixture sizes its rings by slot count, so it still skips *there*, with a reason about that arena rather than about the system. **A `TFT007` `pass` therefore always means at least one edge was compared** — the second skip condition closes a review finding where a declared-but-unmeasurable arena (`doctor` at bringup, or a publisher that has stopped) reported `pass` having compared nothing, with no note either. **What §6's amendment states and does not solve:** `--discover` writes a *measured* rate into the same `rate_hz` the arena reads as an *intended* one, so a recording of a degraded publisher declares the fault as nominal — a discovered rate is a starting point to review. **`doctor` now reads a recording**, which is where `TFT018` and `TFT019` reach a verdict at all: `--from-bag <recording.mcap>` ingests through `tf_tree_ingest::run` and replays the recording's own log order, `--from-file <index.tft>` opens a frozen arena through `Tree::open_frozen`, and both hand the catalogue an ordinary `Tree`. **`TFT018` skips on every *arena* source and not merely on a live one** — §6's amendment records that the live-arena rule was keying on the wrong fact, since a ring holds only the pushes `SampleRing::push` accepted, so a `.tft` would have passed the check unconditionally — and **`TFT019` inherits that skip** rather than working around it, since the evidence is the same one, and skips a second way: when the edges that went backwards are in no wall-clock domain, naming their tags — and the refusal is now *per tag*, so a `SimDomain` edge is sent to [`0012`](./decisions/0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md) rather than told its clock cannot have stepped. **§6's "concentrated in a short window" is implemented as at least eight consecutive arrivals invariant 6 would have rejected, and the eight is `tf_tree_cli`'s choice rather than this document's** — §6 names no length, `checks::CLOCK_STEP_MIN_REJECTED_RUN` carries the argument and both of its costs, and it is counted in arrivals because the stamps are the quantity under suspicion and the stream carries no independent arrival clock. Below the threshold the check *passes* and discloses the run length rather than calling a stray inversion an NTP step. Where it attributes some edges and not others, that is disclosed in `Meta.notes`, the same mechanism `TFT007`'s partial coverage uses. |
| §7 `tf_tree top` | **Done, both halves.** `tf_tree top` exists, attaches read-only and *refuses* `--rw`, and renders all four panes §7 names: per-edge kind/rate/staleness/occupancy/writer, the participant list (arena record ∪ lock-file byte, so read-only participants appear at all), a rolling feed derived from counter deltas, and a per-edge detail view with an inter-arrival histogram. **Built with plain ANSI, not `ratatui` — see the amendment below.** `--web` serves the same [`Sampler`] over a hand-rolled HTTP/1.1 loop on `std::net::TcpListener` (**no new dependency**; a server crate is the third instance of §7's own argument, and the web-view amendment below records it), binding `127.0.0.1:8787` by default. One embedded HTML file, one `/api/tick` JSON endpoint (schema `tf_tree.top/1`), hand-written SVG, **no CDN — enforced by a `default-src 'none'` CSP the server sends, not promised by a comment.** Gated by `cargo nextest run --workspace`: the unit tests in `src/web.rs` plus `crates/tf_tree_cli/tests/web.rs`, which runs the shipped binary and parses the document a browser would receive; `just shm-check` runs the latter again under `--features shm`, which is the build an operator attaches with. **Three defects the amendment names were found by writing those tests and are fixed.** **Not done:** `--web` has no keep-alive, caps itself at 64 concurrent connections and is not a general-purpose server; there is no key handling on either half (see the `ratatui` amendment). |
| §8 Visualization | **Deliberately not built** — this is the finished state, not a gap |
| §9 Benchmark artifact | **Partial.** `just bench-report` emits `report/{results.json,index.html}` with the §9.3 provenance header, all eight §9.2 rows, and all four §9.3 "where we are worse" entries; `Report::validate` makes the honesty rules structural — the tool refuses to write a report that over-claims, rather than relying on whoever wrote it. On this host every comparison row is `UNAVAILABLE` with its own reason, which is §9.3's prescribed output, not a gap in the tool. **Two of those reasons had gone stale and were corrected:** the `.tft` rows said §2 and §3 "are not implemented", citing *this table* as the source of truth, while this table records §2 as Done and §3 as done for MCAP — so the tool was printing a false statement under the one section (§9.3) that is about stating a true one. Both reasons are now derived from `cfg!(all(feature = "shm", target_os = "linux"))` — the frozen backend genuinely is not compiled into `just bench-report`'s build — and **two tests pin the general rule: an unavailable row's reason is about this host or this build, never about the roadmap.** **Not done:** §9.1's container image, the public sample recording, `tf_tree bench compare`'s CLI spelling, and §12 gate 7 (reproducing a committed `results.json` on a clean machine). The CLI spelling's blocker is no longer §3 (which landed) but the crate boundary: `tf_tree_bench` is `publish = false` and carries `criterion`, so a shipped `tf_tree` subcommand reaching it would drag a benchmark harness into every install — `CLAUDE.md` routes that to a decision record, not a PR. **§9.1's *measurement* now exists even though its CLI spelling does not** (`just dds-bench`, `ros/tf_tree_bench_ros`): one publisher, real DDS, §5.2's QoS, N `tf2_ros::TransformListener` consumers against the ingest bridge, warm-up discarded and stated, and the whole input set — publisher plan, bridge topology, query set — *generated* from one `tf_tree_bench::workload` entry so §9.3's "identical data" is structural rather than promised. Measured on this host at 4 consumers, in four arms — the two tf_tree ones, the ordinary tf2 deployment, and tf2's *best* case (one composed listener, in the table so the comparison is not a strawman). **The figures live in `docs/benchmarks/tf2.md`'s §9.1 section and are deliberately not restated here.** This row used to carry its own p50 / p99.9 / PSS triple per stack, and not one of those nine numbers survived the run that produced the four-arm table — the same run whose CPU column replaced an instrument that had been reading the main thread's `schedstat` while every arm did its work on other threads. A status table that keeps a private copy of a measurement keeps a copy that goes stale, and this one did. **The arm this row used to say could not exist now runs.** Until [`0015`](./decisions/0015-the-bridge-fills-a-shared-arena.md) landed, `tft_bridge_create` built a *heap* arena with `TreeBuilder::build()` that no second process could attach to, and `dds_report` printed that gap above its own table on every run; this row said the same and called `0015` a **draft**. Both halves are stale: all eight of `0015`'s steps are implemented — it is still **`ready`** rather than `implemented` for the two reasons its own header names, the fork test and §9.2's N = 1…16 curve, and neither of those is this arm — and `just dds-bench` reports **four** arms, the fourth being one bridge process publishing a shared arena under `$TF_TREE_NAME` plus N processes attached to it read-only through `tft_tree_open()`, at 0 % lookup failures. The bridge's CPU and PSS are summed *into* that row and divided by the consumers it serves — it reports `consumers 0` — so the extra process is charged rather than hidden, and the `procs` column shows the N+1. `docs/benchmarks/tf2.md`'s §9.1 section carries the table, the arithmetic and the two places tf_tree is *worse* in it (total PSS at N = 4, and both `.processes` arms' `svc` tails on an unpinned host). That closes §9.2's *total RSS across N consumers* row for a bridge-filled arena and makes §12's criterion 4 demonstrable on the online path, and it leaves this section's remaining gaps the ones listed below rather than the arm itself. **Building it found a real bridge defect**, recorded in `docs/benchmarks/tf2.md`: `tf_tree_bridge::Publisher` is keyed on the resolved *node name* rather than on the GID, so messages arriving before the graph cache resolves claim the edge under an unknown name and `first_writer_wins` then rejects the real publisher permanently — 9 864 of 10 070 transforms dropped and 100 % of lookups failing against a single correctly-declared publisher. Also **not** part of §9 and deliberately not gated: a broader exploratory suite (`just contended-scaling`, `just scale-sweep`, `just soak`, `just bench-run`/`just bench-ab`) covering §11.2's writers-and-pinning row, the width/depth/ring/fan-out axes, multi-minute drift, and A/B comparison of two builds. Those emit `tf_tree.bench-run/1` rather than joining `REQUIRED_ROWS`, because this host fails `Fitness::probe` and a gate that flaps is a gate people learn to ignore. |
| §10 Open-source readiness | **Partial.** Name decision made, measured and recorded ([`0008`](./decisions/0008-the-name-tf-tree.md)): `tf_tree` is free on the crates.io sparse index and is kept there; **PyPI refused it** as too close to the existing `tftree`, so the distribution is `transform_tree` while the module stays `tf_tree` — an earlier revision of this row said the name was free on PyPI too, which `README.md` and `just artifact-versions` both contradict. `LICENSE-MIT`/`LICENSE-APACHE`, `NOTICE`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md` (real address, and an explicit in-scope/out-of-scope boundary around §3.10's trust model) and `SUPPORT.md` (response expectations, platform support, MSRV policy) are in place; `README.md` is rebuilt against the §0.0 tables. **The MSRV was measured and was wrong:** `rust-version = "1.83"` could not build — `blake3` pulls `constant_time_eq 0.4.2`, edition 2024 — so the floor was raised to 1.85 and is now **1.87**, with a CI job that reads it from the manifest and builds `--locked` on exactly it. `just msrv`'s third arm exists because the first two passed while the README said 1.85 and the manifest said 1.87; this row was the same kind of stale copy and is corrected here. Every `publish = false` crate now states its reason in its own manifest. **The benchmark artifact is now a regression gate** (`just bench-check`, CI job `bench-gate`): `crates/tf_tree_bench/baseline/results.json` is committed, and `bench_report --check-baseline` fails on a withdrawn claim, a dropped §9.2 row, a changed `layout_hash`/`format_version`/build profile, or a directional metric past the slack the baseline itself records. **The comparison ignores every host fact by construction** — CPU model, cores, kernel, governor, load and all `reason` prose — because a gate that fails for the CPU model is a gate people learn to ignore; `src/baseline.rs` carries the split. Making that possible needed `results.json` schema `/2`: `/1`'s bare `{value, unit}` gave a consumer no way to know which direction was an improvement. **On this host the gate holds exactly one number** — the LerpSlerp differential's `max_deviation`, the one row that is host-independent by construction — and that is not a placeholder: `Report::validate` now refuses any row that prints numbers while giving none of them a direction, so a row that becomes measurable arrives gated or not at all. **`docs/PHASE2.md` §11.4's `shm_torture` now exists** (`just shm-torture`, 30 minutes, six processes, `SIGKILL` at 6 Hz) with `just shm-torture-asan` for §11.4's "under ASan" and `just shm-torture-self-test` — the seconds-long half that runs in `just shm-check`. It asserts three things, and the second and third are there because the first revision of this harness had none of them and was **vacuous on most seeds**: that an injected corrupt transform is caught by a process that did not write it; that a run validating too little *fails* instead of printing the same `0 violations` a healthy one does; and that `--inject-violation` finishing clean is itself a failure. **The killed processes are joiners, never the rendezvous owner** — see `docs/PHASE2.md` §0.0's §3.5 row for why, and the §11.4 row for what that costs. **The sanitizer rows are wired to recipes that were run on this host, not to a green tick:** `just tsan` (passes), `just shm-torture-asan` (passes on the fixed harness: 152 936 checked reads — 122 344 of them composing all four edges — 477 kills, no ASan report, over 478 observation rounds), and `just cpp-check` for the C++ UBSan half. **There is no Rust UBSan row and its absence is deliberate:** `rustc -Zsanitizer` accepts address/thread/leak/memory and the CFI variants and has no `undefined`, so §10's "ASan/UBSan/TSan" is a C/C++ checklist and its UBSan half lives where there is C++ to check. The nightly workflow (`.github/workflows/nightly.yml`) carries the torture and sanitizer jobs. **Not done, and this list was wrong in two of its three items until 2026-08-29** — its premise, "all three are ceremony **until there is a release**", expired on 2026-08-17, when the project began publishing to crates.io and PyPI. Corrected: **release automation exists and has run.** `.github/workflows/release.yml` publishes the five crates by crates.io Trusted Publisher (OIDC, no stored token) and `wheels.yml` publishes the wheels; both trigger on `v*`, so one tag drives both. `wheels.yml` has run five times and was green on `v0.0.3` (2026-08-19) and `v0.0.4` (2026-08-22), every wheel row included. **PEP 740 attestations are published** — `wheels.yml`'s `publish` job carries `attestations: write` and `attestations: true` — so naming them here as absent was false; `PHASE3.md` §14's checkbox for them was unticked for the same reason and has been split. **`cargo-dist` is absent; prebuilt binaries are not, and this row's previous argument for skipping them was a non-sequitur.** It read: "`cargo-dist` is absent and is not owed — it builds and uploads binaries for a *binary* release, and the only binary here (`tf_tree_cli`) is `publish = false` by decision." `publish = false` is a statement about the **crates.io index**, and `tf_tree_cli`'s own manifest says its reason is mechanical — three of its dependencies are path-only, so there is no version to publish against — and explicitly *not* a claim about what has landed. Nothing in it bears on whether a user should be handed a binary. The cost of that inference was four tags (`v0.0.1`–`v0.0.4`) shipped to crates.io and PyPI with **no GitHub Release at all** — a tag push creates a tag ref and nothing else — so the audience §4 and the README both put first, somebody holding a recording and no Rust toolchain, had `git clone && cargo install` as the only entry to `doctor --from-bag`. **`release.yml` now builds four Linux rows** (`{x86_64,aarch64}` × `{gnu,musl}`, every one a native runner) through `just release-archive`, and a `github-release` job attaches them with a `SHA256SUMS`. `cargo-dist` itself stays absent on §10's own "or equivalent": it *generates* the workflow from its config and regenerates it on upgrade, and every other job in these workflows carries the argument for its own shape, which a generated file cannot. **Four properties are gated inside the recipe rather than asserted here**: the binary is *executed* and its `--version` compared to the workspace number (a cross-build, a truncated write and a stale artifact all produce a plausible file); the archive is unpacked and re-run through the `tft` symlink; the licence texts travel with it, as Apache-2.0 §4(a) requires and as the crates.io tarballs are already checked for; and packaging is byte-deterministic across mtimes, ownership, **modes** and the gzip header. **Three of those checks were vacuous when first written and are recorded as such in the recipe** — packing twice inside one second cannot see a `date`-derived mtime; gzip zeroes its MTIME field for pipe input whether or not `-n` is passed; and the mode differential did not exist at all, so the builder's umask reached the archive and one commit checksummed two ways on a developer's box against a runner — which is `docs/PROJECT.md` §6's anti-vacuity smell caught in this file rather than after it shipped. `--sort=name` remains untested and says so. **The SBOM per release now exists**: `scripts/sbom.py` writes CycloneDX 1.5 from `cargo metadata`, `release.yml` attaches it and covers it with `SHA256SUMS`, and `just sbom <version>` produces the same file locally. It is written from `cargo metadata` rather than by adding `cargo-cyclonedx` for the reason this row already gives for not using `cargo-dist` — a generated artifact cannot carry the argument for its own shape. **Its scope is the shipped graph, not the workspace**: walked from the five publishable crates plus `tf_tree_cli` over `normal` edges only, so `criterion`, `proptest` and the rest of the dev graph are absent — asserted, not assumed. Deterministic by construction (no clock, no random UUID; the serial number is derived from the component set), so two releases diff to their dependency change and nothing else. **Signed tags are half-done and the half that is missing is a key, not code**: `release.yml` checks the tag object for a signature and **warns**, becoming a refusal when the repository variable `REQUIRE_SIGNED_TAGS` is `true`. Warning rather than failing is deliberate — a gate that must exist before a key does cannot also block the next release on that key — and all three paths are exercised: a signed annotated tag passes, an unsigned one warns, a lightweight tag (which has no object and can never be signed) warns. `CONTRIBUTING.md`'s *Releasing* section carries the one-time setup. So what remains is the mdBook site and a signing key; the honest reason for the first is that nobody has asked for it. |

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
- **CI produced no run between 2026-07-23 and 2026-08-16**, and runs again
  since. Every gate claimed in this document is still run locally through
  `just` with the arch stated: CI covers what its jobs cover, and this
  document's benchmark rows in particular are host-dependent in ways no runner
  settles.

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

> **Disputed, 2026-08-22, by [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md) — `draft`, so this clause is flagged rather than retracted.** The header fields above exist; the **region table does not**. `crates/tf_tree_arena/src/layout.rs` declares `N_REGIONS = 11` and `grep -n spline` over it returns nothing, so a Phase 6 spline region is a *twelfth* region and changes `ArenaLayout::total_size()` for the same declared geometry — i.e. another `FORMAT_VERSION`. `spline_region_off` is a place to write an offset, not a reservation of the bytes it would point at. Do not cite this clause as the reason a byte can wait until it is settled.

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
> so `PHASE3.md` §6.1's estimate under-shoots it — but the same measurement shows
> the estimate under-shoots the **pose** row too, so a layout-dependent
> multiplier would correct the smaller of the two errors and leave the larger,
> and the estimate is deliberately not given one. **Both residuals are priced in
> one place and not here**: `PHASE3.md` §6.1's amendment is the single account of
> `NS_PER_STEP_ESTIMATE` — where the number comes from, what it under-shoots and
> by how much, and why both sides of the threshold stay far below CPython's
> switch interval regardless. An earlier revision of this block was a third
> independent write-up of that arithmetic, quoting a "~2x" ratio and an
> outstanding NORMATIVE instruction; the constant has since been re-derived
> (55 → 64 ns/step, [`0013`](./decisions/0013-the-benchmark-gate-never-interpolated.md)),
> which made both claims stale — which is exactly what three copies of one number
> do. And
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
> and refusing to run would be far worse than losing a diagnostic. `ArenaView`
> carries a `writable` flag, default `false`, so forgetting to opt in loses a
> counter and forgetting to opt out cannot crash anything.
>
> **The justification this paragraph gave for the loss does not hold for two of
> the checks, and that is corrected here rather than overwritten (2026-08-29).**
> It read: *"`doctor` reports what the writable participants recorded, which on a
> real robot is the bridge and every publisher — the processes whose failures the
> counters are mostly about."* An `EdgeCounter` is incremented by a **lookup**,
> and a bridge does not look up; it publishes. `TFT010` is *"an edge whose
> consumers keep asking outside its window"* and `TFT011` sizes a ring against
> *consumer* lag — both are about the processes that read, and those are exactly
> the ones D18 makes read-only by default. So on the ordinary deployment — one
> publish-only publisher, N read-only consumers — the two consumer-facing checks
> see nothing at all while consumers are hammering the arena.
>
> The design still stands: a `PROT_READ` mapping cannot write a counter, and the
> `SIGSEGV` this paragraph opens with is why. What changes is the **disclosure**.
> `no_counter_evidence`'s skip reason names the read-only cause and points at
> `tf_tree participants` for the rw/ro split, so an operator is not sent looking
> for a bag-shaped explanation of a deployment-shaped state. Giving a read-only
> consumer a writable counters region is a different question — a second mapping
> or a per-participant region — and it is a decision record, not an amendment.

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
| `TFT004` | Clock skew between publishers | warn | per-publisher `ClaimRecord::clock_offset_nanos` (see §6's amendment) |
| `TFT005` | Stamps in the future | warn | newest stamp vs now, per edge |
| `TFT006` | Zero or absurd stamps | error | value check during ingest and push |
| `TFT007` | Publish rate deviates from nominal | warn | derived from stamps vs `nominal_rate_mhz` |
| `TFT008` | Jitter: p99 inter-arrival ≫ nominal | warn | derived from stamps |
| `TFT009` | Gaps / dropouts, **including the one that has not ended** | warn | derived from stamps; the trailing gap `now - newest` needs a live arena and a wall-comparable clock, and discloses when it could not run |
| `TFT010` | Extrapolation hotspot | warn | `EdgeCounters` + participant attribution (skips on an arena that has served no lookups — see the amendment below) |
| `TFT011` | Ring capacity too small for observed consumer lag | warn | worst extrapolation gap vs buffer span, **or** `capacity × period` vs observed publish latency; skips only when neither has evidence |
| `TFT012` | Disconnected subtree | error | topology walk |
| `TFT013` | Frame declared but never published | info | head == 0 after a grace period |
| `TFT014` | Participant or claim slot leak | warn | Phase 2 lock file vs arena records |
| `TFT015` | Arena occupancy > 80% (frames, edges, participants) | warn | header counters |
| `TFT016` | THP disabled, or `RLIMIT_MEMLOCK` below arena size | info | `/sys`, `getrlimit` |
| `TFT017` | Dynamic edge with no live writer | warn | claim table (added by the amendment below) |
| `TFT018` | Stamps arriving out of monotonic order | error | observed push stream (added by the amendment below) |
| `TFT019` | A wall-clock domain stepped backwards — `TFT018`'s cause, not a publisher fault | warn | `TFT018`'s evidence + the edge's domain tag (added by the amendment below) |

Output modes: human (default, coloured, grouped by severity), `--json` (stable schema, for CI), and `--exit-code[=error|warn]` so `doctor` can gate a robot's startup or a CI job.

> **Amendment (2026-08-29): `--exit-code` gained a `warn` tier, and the reason is that its error tier is narrower on a live arena than it reads.** Six ids carry `Error`, and on a live arena four of them structurally skip — `TFT001`, `TFT002`, `TFT003` and `TFT018` all need evidence an arena does not carry — so `--exit-code` reduced to `TFT006` (impossible stamps) and `TFT012` (cycle or disconnected subtree). Those are the right *errors*; both make every lookup fail. But almost everything an operator is paged about is `Warn`: a dynamic edge with no live writer, an undersized ring, rate collapse, gaps, clock skew, a slot leak, an arena at 100% capacity. All of it exited 0.
>
> **The capability was already there and only the exit code was missing.** `doctor --json | jq -e '.summary.warn == 0 and .summary.error == 0'` gates on exactly that today, and §6 names `--json` as the CI mode; `Report::is_healthy` was written and unit-tested for it and had **no caller**. Bare `--exit-code` still means `error`, so no existing invocation changes, and `warn` is *warn-and-above* rather than warn-only — an arena with a cycle must not pass it because nothing warned. `--suppress` is the escape hatch for a warn a particular fleet has decided to live with.

**`TFT004` deserves special care** — it is the check most likely to find something nobody knew. Compute per-publisher offset between header stamp and arena receipt time, track a rolling median, and report publishers whose median differs from the fleet median by more than a threshold. On a multi-machine robot with imperfect PTP this finds real problems that present as intermittent extrapolation errors.

> **Amendment (2026-08-26, [`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md)): do not compute the offset here. It is already computed.**
>
> The paragraph above says to difference a *receipt time* against the header stamp. **An implementer who does that reproduces a ±1 s noise floor**, and the amendment exists because one nearly did. The write is sampled — one per second of published data — so a stored receipt belongs to the last *sampled* push while the ring's newest stamp belongs to whatever has been published since. Measured, on a 10 Hz publisher whose clock is **exact**: `receipt − newest_stamp` reads `+3 µs` on the sampling push and `−900 ms` nine pushes later, decided by nothing but when `doctor` arrives. The interval is ~1 s for every publisher by construction, so it does not cancel in the fleet comparison either.
>
> **The writer does the subtraction**, because it is the only party holding both sides at one instant. `ClaimRecord::clock_offset_nanos` **is** the per-publisher offset. Read it; do not difference it against anything.
>
> Three further facts the record establishes and this section did not anticipate:
>
> - **`0` means *no sample yet***, and the sampler never writes a `0` — an exact-zero offset is stored as `1`, because on a host whose clock is coarser than a push a self-stamping publisher produces exactly zero every time and would read as never-sampled forever.
> - **Only `SystemDomain` (tag 0) edges record anything.** `wall clock − stamp` is an offset only where both share an epoch; a `SimDomain` edge would record ~1.79 × 10¹⁸. `TFT005`'s skip is per-*arena* and cannot express one tree holding both.
> - **Four skips, not the three the plan named**: `== 0`, `TFT005`'s epoch condition, a frozen `.tft`, and a **replayed** source — bag ingest publishes through the same `EdgeWriter`, so a 2024 recording read in 2026 records a two-year offset that is arithmetically right and diagnostically meaningless.
>
> **Amendment 2 (2026-08-27, `0036` step 3): the fleet comparison this section opens with is not implementable from one sample, and the reason is not the one amendment 1 gave.**
>
> Amendment 1 above says the rolling median stays unbuilt because it needs a polling loop `doctor` does not have. True, and not the binding reason. **A recorded offset is the publisher's clock error *plus* its stamp-to-push latency, and one sample cannot separate them.** A localiser that stamps a transform with the capture time of the scan it matched legitimately sits tens of milliseconds above an odometry publisher that stamps at computation time, so a fleet-relative rule reports that healthy difference as skew **however well calibrated its threshold is** — the quantity it compares is not the quantity it names. A fleet to measure a threshold on would not have fixed it.
>
> **What separates them is drift**: a clock error moves over time and a pipeline latency does not. That needs a series, which `tf_tree top` polls for — and `crates/tf_tree_cli/src/top.rs`'s module header carries the owed rule, beside the loop that would collect it, rather than only here where whoever adds that column has no reason to look.
>
> **So `TFT004` as shipped fires on one thing**: an offset past `checks::OFFSET_BEYOND_ANY_PIPELINE_NS` — ten seconds, where latency is no longer an available explanation. That is a *physical* argument rather than a calibrated constant, which is what makes it permissible where a fleet-relative threshold is not. It finds a machine whose NTP never came up or whose RTC is dead; it does **not** find the PTP-scale drift this section opens with. The fleet spread ships as a report note with that caveat attached.
>
> **One gap it cannot close.** `ros2 bag play` into a live stack defeats the replayed-source skip: that skip reads how *`doctor`* obtained the stream, and a bag played through the §5 bridge into a shared arena is an ordinary live arena. Its publishers stamp with the recording's original times, so every edge records an offset of however old the recording is. Provenance is a property of the writer and the arena records none, so the finding text names replay as the alternative reading — the most an unprovenanced arena permits.

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
> *published and then abandoned*; `TFT014` is a slot, or the claim it was
> holding, whose owner is gone, which is not *no claim at all*; `TFT006` judges
> a stamp's **value**, and a stream of entirely plausible stamps can still
> arrive backwards.
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
> > **Amendment — `TFT019` cannot fire on a deployment, and that is stated
> > rather than discovered.** The check needs a *recorded* push stream, and
> > `doctor` has exactly two sources: the built-in reference fixture and a live
> > `--attach`. It skips on the second — the live-arena rule the third bullet
> > above states — so outside the fixture it never reaches a verdict at all.
> > **`TFT018` is in the same
> > position for the same reason** — the live skip is where both of them stop.
> >
> > This is recorded as a limitation and not a caveat, because a diagnostic that
> > silently never fires is worse than no diagnostic: its silence reads as an
> > all-clear on the exact fault it was written to name. So it is said in the
> > three places it can be met — the live skip reason in the report itself, this
> > amendment, and `RUNBOOK.md`'s `NonMonotonicStamp` section — and none of them
> > may be quietly dropped while the limitation stands.
> >
> > **What would lift it is a third `doctor` source, and that is a feature, not a
> > fix.** `Tree::open_frozen` (§2.1, behind `shm`) already reads a `.tft` back
> > through the ordinary `Tree` API, and `Observations::from_arena` already
> > builds a push stream from any `Tree`'s rings — so a `doctor --from-file`
> > needs a third `Source` variant whose "is this live" answer is **no** (a
> > frozen file has no concurrent writer, which is the entire reason for the live
> > skip) and a flag to select it, not new evidence machinery. It is deliberately
> > *not* in this commit: it changes `doctor`'s input surface, and that is its own
> > change with its own review. Until then, a backwards clock in a recording is
> > diagnosed by `tf_tree ingest --bag`'s per-edge `--clock-reset-threshold`
> > guard (§3), which is a different rule and not this check.
>
> > **Amendment — the limitation is lifted, and the follow-up above named the
> > wrong source.** `doctor` now has two recording sources. `--from-bag
> > <recording.mcap>` ingests through `tf_tree_ingest::run` — the same two passes
> > `tf_tree ingest` and `tf_tree freeze --from-bag` run — and `--from-file
> > <index.tft>` opens a frozen arena through `Tree::open_frozen`. Both hand the
> > catalogue an ordinary `Tree`, so §4.1's "no separate offline API" is
> > structural here too: nothing in `checks.rs` learned a new input shape.
> >
> > **`TFT018` and `TFT019` run on `--from-bag`, and still skip on `--from-file`.
> > The amendment above predicted the opposite, and the prediction was wrong for
> > a reason worth keeping.** It said a `.tft` source needed "a third `Source`
> > variant whose *is this live* answer is **no**", because a frozen file has no
> > concurrent writer. That is true and it is not sufficient: liveness was never
> > the property these checks depend on. `SampleRing::push` **rejects** a stamp
> > older than the ring's last, so a ring holds only *accepted* pushes and
> > `Observations::from_arena` can only ever reconstruct a non-decreasing
> > sequence — from a live arena, from a `.tft`, and from an arena built by §3.1,
> > which additionally *sorts* every edge before pushing. The rejected arrival
> > `TFT018` exists to name is absent from the arena, not merely hard to read
> > there. Wired the way that follow-up describes, both checks would have **run
> > and passed on every `.tft` ever written** — a guaranteed all-clear on the
> > exact fault they detect, which is the failure this section spends a page
> > refusing in the other direction.
> >
> > So the skip is re-keyed from liveness onto the property that decides it:
> > `tf_tree_cli::checks::PushStream`, four-valued, with the predicate and the
> > skip *reason* returned by one function so a new source cannot answer one and
> > not the other. `Observed` (the fixture, which is the publisher and can record
> > what the engine refused) and `Recorded` (a recording's own log order) run;
> > `RingsAtRest` and `RingsUnderWriter` skip, and their reasons differ — a torn
> > window under a writer, an absent rejection at rest — because they are
> > different facts.
> >
> > `TFT001` is re-keyed by the same enum and gains its own recording reason: a
> > `tf2_msgs/TFMessage` has no sender field and an MCAP channel names the topic
> > rather than the node, so **a bag cannot answer the multi-publisher question
> > `PHASE4.md` §1.3 predicts a real stack will fail.** That is stated rather
> > than worked around; §3.2's `static_conflicts` is the nearest thing a
> > recording has, it is counted by the ingest report and it is not this check.
> >
> > What the recording path does **not** carry, reported rather than fixed by
> > adding a field: `Anomalies::out_of_order` is one count for the whole
> > recording with no edge attached, and neither pass retains arrival order —
> > `fill` sorts it away by construction. So the CLI replays the recording a
> > third time through the same `source::read_tf` both passes use, with the same
> > filter `fill` applies. A `tf_tree_ingest` type that carried per-edge arrival
> > evidence out of pass one would remove that pass; it is a change to that
> > crate's output and not to this one's, and it is not made here.
>
> > **Amendment — the recording source shipped a fabricated all-clear of its own
> > in `TFT010`, and the fix is keyed on the evidence rather than on the
> > source.**
> >
> > An arena built from a recording has been **written and never read**:
> > `tf_tree_ingest::run` pushes and performs no lookup. Every `EdgeCounters`
> > field is therefore zero — and zero extrapolation errors is *precisely* what a
> > healthy, heavily-exercised arena looks like. `TFT010`, whose evidence is
> > entirely those counters, walked the empty set and reported `pass`.
> > `TFT011`'s counter half did the same, and its other half was already
> > structurally silent on a recording (no arrival delay), so the whole check
> > reported `pass` after examining nothing. That is the same defect this section
> > already corrected once for `TFT007` — an observed rate compared against an
> > *undeclared* zero — arriving in a new place.
> >
> > **Both now skip, naming the reason.** The predicate is
> > `tf_tree_cli::checks::no_counter_evidence`, and it is read off the counters
> > themselves — `lookups_ok + err_extrap_before + err_extrap_after` summed over
> > the arena — rather than off the `Source`. Three consequences, all wanted:
> >
> > * A fifth `Source` cannot silently reintroduce the bug; there is no source
> >   list to forget to extend.
> > * The **reference fixture** is in the same state (`fixture::spin_up`
> >   publishes; nothing calls `Plan::at`), so `TFT010` skips there too. `tf_tree
> >   doctor` now reports `11 passed, 2 fired, 6 not run`, and §0.0 carries the
> >   new count. The old `12 passed` included a row that had measured nothing.
> > * A **live arena at bringup**, before its first consumer, gets the same
> >   honest skip — which is the case an operator is most likely to run `doctor`
> >   in.
> >
> > The threshold is *any* lookup, not *enough* lookups: one is what makes a zero
> > mean zero rather than unknown, and a significance threshold is not something
> > this document states. The `counters`-feature skip keeps its own separate
> > reason, because "rebuild the engine" and "exercise the arena" are different
> > instructions.
> >
> > **`TFT011` skips only when *both* halves are blind**, since either alone is a
> > real result; the surviving half is disclosed in `Meta.notes`, the mechanism
> > `TFT007`'s partial coverage already uses.
> >
> > **`TFT017` is the mirror image and is deliberately *not* skipped.** A
> > bag-built arena has no writer at all, so the *unclaimed dynamic edge* warn
> > fires on every edge of every healthy recording. It stays a warn because a
> > fleet whose publishers have all died reaches the identical arena state, and
> > falling silent there would delete the check's purpose; instead, an
> > all-unclaimed arena earns a `Meta.notes` line saying the finding names the
> > arena rather than any edge in it. **Open question, recorded rather than
> > invented:** whether a `Recorded`/`RingsAtRest` source should suppress it
> > outright needs a source-shaped predicate this document does not settle, and
> > the naive evidence-shaped one — "no edge carries a claim" — would silence the
> > total-outage case this check exists for.
>
> **Paired with a documentation line, not just a check:** anything published at
> rate should declare a steady or PTP domain rather than the system wall clock —
> `SteadyDomain` (tag 3) since the amendment above, or, `Domain` being an open
> trait, a driver's own unit struct and `TAG` for a PTP-disciplined clock
> (`API.md` §2.5). The check tells an operator what happened; the doc line is how
> the next robot avoids it. `RUNBOOK.md`'s `NonMonotonicStamp` section carries
> both.

> **Amendment — `TFT014` implements the half of its own title it was named
> for. The participant-slot leak is detected; nothing is reclaimed.**
>
> The catalogue row reads *participant or claim slot leak* and only the claim
> half existed. Worse, the claim half was blind in exactly the state the other
> half is about, and for the same reason the underlying defect exists: it fired
> on `owner_pid == 0`, and `owner_pid` came from
> `ParticipantTable::identity`, which answers for any record whose `state` word
> reads `LIVE` — which a participant killed without running `Drop` leaves
> behind, and until something clears it that answer is a dead pid. `PHASE2.md`
> §5.1 says "any code deciding liveness from `state` is a bug" in those words,
> and this was that bug in the tool whose job is to find it. Filed as #184 and
> worked through in
> [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md), whose plan
> step 6 this is.
>
> **Which arenas the slot half actually fires on, because it is not #184's.**
> #191 landed the owner's socket-hangup reap — `0028`'s *candidate B* — so a
> rendezvous joiner `SIGKILL`ed under a running owner has its record released by
> `ParticipantTable::release` in the owner's hangup callback. Measured with an
> owner, a read-write joiner and a third process observing the table: with the
> joiner `SIGKILL`ed, its `state` word had gone `0x6` (`LIVE`) to `0x0` (`FREE`)
> by the observer's first poll 50 ms later, and on that arena the slot half
> correctly says nothing. A slot finding therefore means the slot was one that
> reap **cannot** reach, which is a sharper diagnosis than "nothing reclaims"
> and is the reason the finding's own text names the set. `0028` enumerates it;
> the members that
> leave a `LIVE` record over a free byte are the owner's own slot, an owner
> killed between the hangup's probe and its CAS, a client the owner's
> `epoll::add` failed for, a `ReadWrite` `Tree::attach_shared` participant, and —
> once §3.5 is wired — a takeover heir's inherited peers. **Two of those five
> leave the owner dead, so `doctor --attach` cannot be pointed at them:** the
> rendezvous died with the owner and a fresh join is refused
> `ArenaHeldButUnreachable`. That is a limit on the *source*, not on the check;
> `crates/tf_tree/tests/rendezvous.rs`'s
> `the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live` stages the
> reclaimed peer and the unreclaimable owner on one real arena and asserts the
> refusal alongside them.
>
> **One liveness answer per slot, taken once.** `Snapshot` now carries the
> participant table, each slot with `Tree::participant_alive` already applied —
> `F_OFD_GETLK` on the slot's lock byte for a tree from `tf_tree::open`, the
> `/proc` inference otherwise. Both halves of the check read that one answer, so
> a report cannot call a process alive on an edge line and dead on a slot line,
> and a claim's owner is resolved by joining on the slot the claim word names
> rather than by asking `identity` a question it does not answer.
>
> **No new id, and no new arena field.** `TFT014`'s title already claims this
> ground; a second id would be the second spelling `CLAUDE.md` forbids, and
> `0027` has `TFT020`. Severity stays **warn**, because it is warn in the table
> above. §0.0's detecting count was unchanged by this amendment — it read
> "sixteen" when this was written and reads "seventeen" since `TFT004` joined the
> set in #274 — because `TFT014` was already in it, and what changes here is how
> much of its own row it covers.
>
> **It is detection, and stops there.** `0028` is `draft` and its header exists
> so that no reclamation lands before its predicate is settled; a `doctor` check
> that mutated the arena would be the exact thing that record is holding the
> door against. What an operator gets is the count and the budget: a leaked slot
> is `1 of 64` permanently spent, and there is no way to get one back short of
> stopping every participant so the segment is freed.
>
> **And it skips on `--from-file`, where it used to pass.** §2.3's freeze copies
> the whole arena, participant records included, so every slot in a `.tft` names
> a process that exited when the freeze finished and every claim in it names
> that run's slot. Running there would fire on every correct `.tft` ever
> written, about an arena with no assigner for a leaked slot to wedge. The old
> `pass` was no better: it was a fabricated all-clear about a question the file
> cannot be asked, and it held only because the predicate was reading `state`.
> `checks::SlotTable` is the discriminator — the sibling of `PushStream`, and a
> *different* split, because an ingested bag builds its arena in `doctor`'s own
> process and so has a perfectly answerable participant table while its push
> stream is a replay. This is the ninth conditional skip in §0.0.
>
> **Three conditions inside the title remain undetected, and all three are gaps
> in the evidence rather than in the check:**
>
> * **A `RESERVED` record.** `participant_alive` folds `state == LIVE` in ahead
>   of the byte probe, so it answers "not alive" for a healthy joiner mid-attach
>   exactly as it does for one that died there. Separating them needs the raw
>   byte — `LockFile::probe_participant`, which `tf_tree participants` opens
>   directly and `doctor` does not — and reporting `RESERVED` without it would
>   put a warn on every arena a `doctor` run catches mid-attach. Nothing
>   collects a `RESERVED` record either (`0028` open question 6), so the missed
>   leak is one no answer would repair today.
> * **The fork case** (`0028` §6.2): a child that inherited the mapping holds
>   the parent's byte, so the kernel's answer is *alive* for a process that
>   cannot participate. Naming it needs `/proc` disagreeing with the recorded
>   `(pid, start_time)` as a second, independent fact, and `participant_alive`
>   composes that fact away rather than exposing it. Adding a second liveness
>   spelling in the CLI to reach around it is the failure this amendment is
>   avoiding, not a shortcut it takes.
> * **A claim whose slot has since been re-granted**, which is the one of the
>   three reachable from the ordinary #184 flow. The claim's owner word is
>   `(epoch << 16) | (slot + 1)` and that `epoch` is the `ClaimRecord`'s own
>   per-edge counter, not the participant's incarnation, so nothing in the word
>   says which *occupancy* of the slot took the claim. The hangup reap frees a
>   dead writer's slot but not its claims — nothing calls
>   `Tree::reap_participant` on hangup — so once a later joiner is granted that
>   slot the stale claim joins to a live participant and the edge reads healthy
>   while nobody is writing it. **Not a regression:** the `owner_pid == 0`
>   predicate was silent here too, for the same reason. Closing it needs the
>   incarnation inside the claim word, which is an arena format change, and
>   `FORMAT_VERSION = 3` is not reopened opportunistically (`CLAUDE.md`);
>   `0028` does not propose one.
>
> **And one false positive is accepted, named here rather than papered over:** a
> `ReadWrite` `Tree::attach_shared` writes an arena record and takes no lock
> byte, so its healthy participant reads as a leak. `0028`'s open question 1 was
> answered by the owner — the fd-passing attach is for readers, every supported
> read-write participant joins through the rendezvous and takes its byte before
> the record leaves `FREE`, so **the byte is a total predicate** over the
> deployment model. Making that arm refuse is a step of `0028`, not of this
> check.

> **Amendment — `doctor` opens the lock file, and two of the three silences
> above become findings.**
>
> [`0028`](./decisions/0028-the-slot-a-killed-participant-keeps.md) plan step 6,
> landing after that record went `ready`. The amendment above closes with three
> conditions the check could not reach; **two of them were gaps in what `doctor`
> would ask, not gaps in the arena**, and asking is the whole change. `doctor`
> now opens the rendezvous lock file on `--attach` — the thing
> `tf_tree participants` always did — probes every participant byte and reads
> every identity record beside it, and `checks::slot_leak` composes the three
> facts in one place.
>
> * **`RESERVED` is reported**, when its byte is free. It could not be before
>   because the only fact available was `Tree::participant_alive`, which folds
>   `state == LIVE` in ahead of its probe and so answers *not alive* for a
>   healthy joiner mid-attach exactly as for one that died there. With the raw
>   byte the two separate: byte held is a registrant in flight and byte free is
>   a registrant that is not coming back. ~~Note that **nothing reclaims a
>   `RESERVED` record even now**~~ — **that stopped being true on 2026-08-21.**
>   `0028` question 6 widened `reclaim` to accept any observed word, and steps 3,
>   4 and 5 have all landed, so a `RESERVED` record with a free byte is now
>   collected by the assigner when a grant walks past it, by the owner's
>   socket-hangup callback, and by any read-write participant calling
>   `Tree::reap_participants`. What this check buys is still a *name* for the
>   state — the repair is elsewhere and deliberately so, per the detection-only
>   rule below — but the repair now exists.
> * **The fork case is reported, as its own finding with its own message.** Byte
>   *held*, recorded pid gone: a forked child inherited the parent's open file
>   descriptions, so the socket never hangs up and §6.2's rule keeps the lock
>   byte held on behalf of a process that no longer exists. **It is deliberately
>   not the same message as a free byte**, because the responses are opposites —
>   a free byte is a slot a reaper should collect, and this is a slot no reaper
>   may touch, since the kernel's own answer is *held* and overruling it with a
>   `/proc` inference is the inversion §5.1 exists to forbid. The remedy is
>   upstream: stop the child, or use a start method that inherits no descriptors
>   (`multiprocessing`'s `spawn`, or fork+exec). `0030` closes it at the source.
> * **And it is judged from the lock file alone, so the *read-only* inheritor is
>   reported too.** That one is the case worth having: D18 makes read-only the
>   consumer default, a read-only participant writes **no** arena record at all,
>   and Python's `multiprocessing` forks by default — so the likeliest fork leak
>   on a real deployment is a held byte over an arena row that reads `FREE`. A
>   predicate that opened with *"a `FREE` record is not a leak"* returned
>   `"status": "pass"` for exactly that shape while `docs/RUNBOOK.md` told the
>   operator this check reports it; clause (b) therefore reads the byte and the
>   identity record, which are the two facts such a slot has, and the finding
>   names the missing record rather than pretending to one.
> * **The third silence stands unchanged** — a claim whose slot has since been
>   re-granted still needs the incarnation inside the claim word, and no format
>   change is proposed.
>
> **The `/proc` half is three-valued, and that is the load-bearing detail.**
> `Identity::matches_running_process` exists for exactly this sentence and is
> two-valued: it maps *every* read failure to `false`, so on a host whose `/proc`
> is not mounted every participant reads as gone — and the fork arm, which fires
> on *byte held plus process gone*, would then fire on every healthy slot in the
> table. `0028` works that inversion through in *"the fail-safe claim is false on
> this code"*. `recorded_given` in `tf_tree_cli` therefore answers
> *running / gone / cannot say*, and it is `tf_tree`'s own `alive_given`
> transposed rather than a second classification: same three inputs, same arms,
> same bias. **Only one arm proves death** — `ENOENT` on a host that would have
> shown us an entry, tested the way `tf_tree`'s `proc_answers_here` tests it, by
> reading `/proc/self/stat`, which is about a process that is running by
> construction. Every other failure is *cannot say*: an `EACCES` from a `hidepid`
> mount, an `EMFILE`, a `stat` line the parser did not understand. The first
> revision of this step collapsed all of them to *gone* whenever `/proc/self`
> read, which on a hardened host reports a running publisher as a fork inheritor
> — a false death in the tool an operator uses to decide what to kill, which is
> the direction `record_is_alive`'s doc calls corruption.
>
> **And there is a fourth input class, which is not about `/proc`'s answer but
> about whether the question is askable — [`0033`](./decisions/0033-the-identity-record-cannot-name-a-namespace.md).**
> A recorded pid is namespace-local. Resolved against another PID namespace's
> `/proc` it names an unrelated process or none at all, and until `0033` both
> outcomes reached *gone*: a healthy participant inside a container or an
> `unshare --fork --pid` was reported as a fork inheritor, with the *stop the
> child* remediation — the opposite of what that fault wants. The two faults are
> not merely similar in summary, they render **byte-identical** text from the
> same formatter, so nothing in the finding could have told them apart.
>
> `recorded_given` therefore answers *cannot say* ahead of the whole `/proc`
> classification, on two conditions, and **no new id and no new arena field**:
> the recorded PID namespace differs from the observer's own (`Identity` carries
> it since `0033`; `0` is *unknown namespace* and keeps the older behaviour), or
> the observer's `/proc` does not describe the observer's own namespace at all,
> in which case no pid in the file is resolvable **including `doctor`'s own** —
> the shape in which `doctor` reported its own participant slot as a leak. Both
> land on the existing *byte held, cannot say* silence, so a same-namespace fork
> inheritor observed from its own `/proc` is reported exactly as before, and the
> one class that stays undetectable is a participant in a different namespace
> whose byte really has been inherited: this reports nothing rather than
> reporting the wrong thing. **The namespace is recorded at registration, never
> derived at diagnosis** — reading `/proc/<recorded_pid>/ns/pid` fails *open*,
> because in the observer's namespace that pid names a different process whose
> namespace then matches.
>
> **The finding prints the pid its evidence is about.** That is the lock file
> identity's, not the arena record's: on a `RESERVED` row the record's `pid`
> field is still zero (`fill_slot` writes it after the `FREE -> RESERVED` CAS)
> and on a read-only slot there is no record at all, so a subject built from it
> read *"slot 8 pid 0 … /proc has no running process for it"*. Where both exist
> and differ, both are named. The subject also carries which of the two shapes
> it is — `byte free`, `byte still HELD`, or `byte not probed` for a source that
> opened no lock file — because the responses are opposite and the difference
> has to survive being read at 3am.
>
> **The word-before-byte order is pinned by a signature, not by a comment.**
> `0028` piece 2's third constraint requires the `state` word to be observed
> before the byte is probed. `Snapshot::probe_lock_facts` takes a *callback* that
> is handed the already-captured row for the slot it is about, and `slot_facts`
> takes that row rather than a slot number — so there is no lock-file value in
> `cmd_doctor` that could be computed before `Snapshot::capture`, and the hoist
> does not compile. The argument that the order *matters* stays where it is
> proved, in `loom`'s model of `tf_tree`'s `reclamation_verdict`; no sequence of
> stable slot states can show which read went first, so no test in the CLI
> claims to.
>
> **The false positive named above has had its producer removed.** `0028` step
> 0b made both `Tree::attach_shared` and `Tree::attach_shared_at` refuse
> `AttachMode::ReadWrite`, so a byte-less writer has no in-tree producer left.
> `TreeBuilder::build_shared` called directly still registers without a byte and
> is still supported, but such a tree has no lock file at all — it reaches the
> *byte unknown* row of `slot_leak`'s table and is judged by `/proc` alone,
> exactly as it was before. That row is also what `--from-bag` and the in-process
> fixture take, so a source with no rendezvous keeps the predicate this check
> shipped with rather than falling silent, and its message says so instead of
> claiming a probe the run never made.
>
> **Still detection, and still no new id.** Nothing here reclaims anything:
> `0028` reclaims from the assigner (step 3) and from `Tree::reap_participants`
> (step 5), and a `doctor` check that mutated a robot's arena as a side effect of
> being asked a question would be the tool overstepping in the direction D18
> exists to define. `TFT014`'s title already claims this ground, `0027` has
> `TFT020`, and §0.0's detecting count was unchanged by this amendment (it read
> "sixteen" when this was written; #274 later moved `TFT004` into the set).

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

> **Amendment — there are two embedding measurements, and only the row above is gated.**
>
> Both are produced by `just embed-cost`; the design is `crates/tf_tree_bench/src/embed.rs`. They were separated because a first implementation shipped the second one under the first one's title, which answered "what does the embedder's profile cost" while claiming to answer "what does crossing the crate boundary cost".
>
> | | measurement | how | status |
> |---|---|---|---|
> | 1 | **the row above** — separate-crate vs in-crate, depth 3 | **one build, one profile**: two identical `#[inline(never)]` bodies, one compiled in `tf_tree_bench`, one in `tf_tree_core` (`bench_probe`, a default-off feature). Read off the `[profile.embedder]` run, as this section requires; the `[profile.release]` run is the control. | `embedding_cross_crate` in `results.json`, **gated at 5%** |
> | 2 | **profile comparison** — the same out-of-crate body under `[profile.embedder]` against `[profile.release]` | two builds, two processes | **exploratory**, printed by `just embed-cost`, deliberately **not** in `results.json` and not gated — §11.2's shape |
>
> Row 1 is gated on `boundary_ratio`, `out_of_crate_ns` *and* `in_crate_ns`, not on the quotient alone: a change that moves both halves the same way would pass a ratio-only gate, and one that has been measured here (removing `#[inline]` from `Plan::fold_at`) takes the ratio from 1.253 to a *passing* 1.001 by regressing `in_crate_ns` 6.7% while `out_of_crate_ns` improves. `in_crate_ns` is the metric that fires on it. Row 2 is exploratory because it is two processes seconds apart: over runs in which row 1 moved by 0.004 (1.250 → 1.254), row 2 moved between 1.188 and 1.235.
>
> Row 1's verdict is `unresolved` — never a pass or a fail — when the observed round-to-round band straddles the 5% threshold. A gate whose noise floor exceeds its threshold is not a gate.
>
> **On the development host the criterion is NOT met: 1.250–1.254× across three runs** (out-of-crate 240.0 ns against in-crate 191.3 ns, `[profile.embedder]`), reported rather than engineered around. The control says what closes it: the identical comparison under `lto = "thin"` measures 0.994–0.996×. That host fails `Fitness::probe`, so the row is `unavailable` in the committed baseline and none of those figures is a claim in §9.3's sense.

### 9.3 Honesty requirements — NORMATIVE

The first skeptical reader will look for a thumb on the scale, and finding one ends the project's credibility permanently.

- Identical QoS, identical executor configuration, identical DDS vendor and version, all recorded in the report.
- Both stacks warmed; discard the first N seconds; state N.
- Report `tf2` version, ROS distro, RMW implementation, kernel, CPU model, and THP setting.
- **Report where `tf_tree` is worse**, in the same table and not in a footnote: arena memory floor (an idle arena costs more than an idle `tf2` buffer), attach latency, the operational cost of a format bump, and the bridge as an additional process to supervise.
- Publish the harness source in the same repository. No private benchmark.

If a row cannot be measured fairly, omit it and say why. An honest gap is worth more than a favourable number nobody trusts.

**Amendment — "fairly" is three questions, not one.** As first implemented, one `Fitness::probe` verdict governed every row, so a row was refused whenever *any* check failed, whatever that check was about. That is wrong in one direction and it is the expensive direction: it withholds numbers the host could always have produced, and it prints a reason that is not the row's reason — which is precisely what the bullet above forbids. `Report::validate` now asks each row the question its numbers actually rest on (`report::Sensitivity`):

| Row reports | Fails on | Survives |
|---|---|---|
| An **absolute duration** (`AbsoluteTiming`) | every check: debug build, SMT, busy machine, governor, unknown core count | — |
| An **interleaved ratio** (`Ratio`) | a debug build, **and a busy machine** | governor, SMT — they land on both arms of a within-round interleave and divide out |
| **Resident memory** (`Memory`) | a debug build, an unreadable `smaps_rollup` | every timing check; Pss is read from `/proc` and involves no clock |
| A **host-independent** figure (`HostIndependent`) | nothing | — |

**Load is not common-mode between these two engines, and the exception matters.** Interleaving cancels a disturbance only when it lands on both arms alike, and these arms are asymmetric by construction: `tf2::BufferCore` takes a mutex on every lookup and `tf_tree`'s read path takes none. Under load the tf2 arm additionally pays lock-holder preemption and the convoy behind it, which the tf_tree arm has no equivalent of — so a busy host does not add noise to the quotient, it **inflates it in our favour**. That is the thumb on the scale this section exists to catch, so `busy` reaches the ratio axis even though the governor and SMT do not.

Likewise, a memory row requires that Pss be *readable*. `self_pss_kib` returns `0` when `/proc/self/smaps_rollup` is absent (non-Linux, some hardened containers), and a silent zero would leave the memory axis fair while a row published zeros as a claim — the same false-PASS-by-silent-fallback the physical-core count already refuses to make.

The **core budget** is split the same way and for the same reason. "Above the core count the rows measure the scheduler" is a statement about throughput and latency; sixteen workers mapping one `.tft` on four cores share exactly the pages they would share on sixteen. So `needs_n_cores` no longer reaches a `Memory` row — which is what makes **§12 gate 4 measurable on a 4-core host at all**, having been blocked by a question it does not depend on.

A debug build is the one check that reaches everything, because it is not a slower program but a different one.

**The `Ratio` axis has its first row, and it is the tf2 comparison.** `lookup_ratio_vs_tf2` times a depth-3 hot lookup on both engines in one process, `LerpSlerp` on both sides, the arms interleaved within every round with the leading arm alternating, and reports the **median per-round quotient** rather than the quotient of two medians. Measured on the development host: **2.47× with a band of 2.457–2.532 (3.0% wide)** — on the same host, in the same run, whose absolute latencies are `unavailable` because the fitness probe fails. That contrast is the justification for the axis existing.

Two things about it are stated in the row's own note and repeated here because they bound the claim. The tf2 column goes through `tf_tree_tf2_sys` and therefore **flatters `tf_tree`** by the residual FFI boundary, ~21 ns / 8% at this depth; the binding-free comparison is `docker/tf2/native_scaling.cpp` and its headline is 2.7×. And `ns_per_lookup` on either side is reported but **never gated** — it is an absolute duration, and this host cannot claim one.

**There are two committed baselines**, `results.json` and `results-tf2.json`, checked by `just bench-check` and `just tf2-bench-check`. They are not interchangeable: the status comparison is one-directional, so a single baseline cut with `--features tf2` would make the default recipe fail on every host without ROS 2 — on the difference between two recipes rather than on the code, which is the trap `bench-check` already documents for `--embed-cost`. Each recipe checks the baseline cut by the matching build, and `bench_report` names the right regeneration recipe in its failure message.

The JSON keeps its `timing_sensitive` field with its original meaning (*this row reports an absolute duration*), so `tf_tree.bench-report/2` does not change shape. Each rule carries a test with a verified mutant in `report.rs`.

---

## 10. Open-source readiness

Phase 5 is where the repository becomes publishable, so this is a deliverable, not an afterthought.

- **Name check before anything else.** Confirm `tf_tree` is available on crates.io and PyPI, and decide deliberately whether the proximity to ROS's `tf` / `tf2` package names helps discovery or invites confusion. Renaming after 1.0 is not an option; renaming now is an afternoon.
- Apache-2.0 / MIT dual (D30), license headers, `NOTICE`, SBOM per release.
- `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md` with a real disclosure address.
- **A stated support policy**, honestly scoped. A small-team infrastructure project dies from unanswered issues more often than from bad design. Say what is supported, what is best-effort, and what the response expectation is. Under-promising is fine; silence is not.
- **MSRV policy** and a CI matrix pinning it.
- Documentation site (mdBook): a first-five-minutes path that works — `pip install transform_tree`, three lines, a real result — before any architecture prose.
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
3. Frozen lookup p50 within **20%** of online (accounting for the deeper binary
   search). **SUPERSEDED by §2.1, and answered by construction rather than by
   measurement — nothing measures this, and nothing is owed.**

   The criterion prices a cost the design was expected to have. Its parenthesis
   says what that cost was: a frozen index searched *deeper* than a live ring,
   so a 20% allowance kept the deepening bounded rather than open-ended. §2.1 is
   **NORMATIVE** that there is no such search — "the frozen read path uses the
   **identical** `Plan::at` code as the online path, against a `PROT_READ`
   mapping. No offline variant of the lookup, no separate index." That is what
   the code does: `Tree::open_frozen` maps the file and hands a `FrozenArena` to
   the same `&dyn Arena` the heap and `memfd` backings go through
   (`crates/tf_tree/src/frozen.rs:119`, `ArenaBacking` at
   `crates/tf_tree/src/tree.rs:697`), so the three backings are, in §2.1's
   words, three ways of holding the same bytes — which is what
   `a_frozen_lookup_is_bit_identical_to_the_live_one`
   (`crates/tf_tree/tests/frozen.rs:181`) asserts, gate 1's three-way
   bit-identity being the same property stated over all three backings.

   So there is no second implementation for a ratio to be taken between, and a
   `p50` comparison here would be one implementation of the lookup measured
   twice, once per backing. **The criterion and the statement that answers it
   landed in the same commit** (#70): this row was never owed, and it read as
   owed only because it was left standing beside §2.1 without a word.

   **What is genuinely different about a frozen tree is residency, not search**,
   and it is not this row's business: a `.tft` is deliberately *not* prefaulted
   — `open_frozen`'s doc comment gives §2.2's reason, that untouched pages
   costing nothing across sixteen workers is the whole wedge — so a first touch
   pays a page fault where a shared-memory attach has already paid it under
   `PHASE2.md` §7.1. That is a first-touch cost rather than a search cost, the
   sharing it buys is gate 4's subject, and folding it into a lookup-latency
   ratio would attribute it to a deeper search that does not exist. **Unlike
   criterion 5, this row is not held by nobody**: it is answered, and there is
   nothing left to hold.
4. **16 workers sharing one `.tft`: total Pss within 1.2× of one worker.**
   **MET — 1.024×, measured for the first time by `just gate4`**
   (`crates/tf_tree_bench/src/bin/frozen_workers.rs`). 16 workers cost **235.5
   MiB** against **229.9 MiB** for one, on a 338 MiB frozen fleet arena (64
   robots × 40 s, 1 537 frames, 1 536 edges). Solving `total(N) = S + N·p` over
   the two rows gives **S = 229.5 MiB shared, p = 0.37 MiB private per worker**.
   Reproduced to three decimals across runs when it was taken; repeated runs
   on 2026-08-19, on a host carrying other work, spread over **1.023–1.026×**
   with `p` between 0.36 and 0.41 MiB. The third decimal moves; the verdict has
   15 % of headroom. Re-derived on 2026-09-04 at **1.024×** (235.6 / 230.0 MiB,
   `p` = 0.37 MiB) by the run that gave the recipe an exit status.

   > **Correction — until 2026-09-04 this criterion was measured and not
   > gated, and the row above did not say so.**
   >
   > `frozen_workers.rs` computed the verdict, printed `PASS` or `FAIL`, and
   > returned `Ok(())` on both: `grep -n 'process::exit\|ExitCode'` over the
   > file returned one line, a `use std::process::{Command, Stdio}` for
   > spawning workers, and nothing else. `nightly.yml`'s `gate4` job runs
   > `just gate4` as its only step, so criterion 4 could have regressed to any
   > value at all and that job would have been green — the same shape as the
   > `abi_cost` failure `docs/benchmarks/EVIDENCE.md` was created to prevent,
   > reappearing inside a job written to close it, and the same shape as
   > [`0023`](./decisions/0023-the-gate-that-could-not-gate.md)'s subject. Every
   > other gated artifact in the register already failed its process
   > (`owner_migration.rs`, `reclaim_latency.rs`, `mp_bench.rs`,
   > `abi_cost.rs`); this one did not.
   >
   > **The verdict now decides the exit status, and the caller decides whether
   > this run is a gate.** `just gate4` passes `--gate` and fails the process on
   > a FAIL; `just gate4-python` does not pass it and still exits 0, which the
   > amendment below requires. `--gate` additionally *refuses* `--python` (the
   > second gated arm the amendment defers, naming the record it would be
   > making) and `--no-touch` (the control, documented below to FAIL at 5.32×),
   > so neither can arrive as a flag pair in a recipe. A `--gate` run that
   > cannot evaluate the criterion — no N = 1 or no N = 16 row — refuses
   > instead of returning 0 with an explanation printed above it.
   >
   > **Nothing about the criterion, the harness or the 1.024× moves**, which is
   > why this is a correction to a status claim rather than a decision record.
   > What moves is that the number is now re-derived by something that can
   > fail: `crates/tf_tree_bench/tests/gate4.rs` drives the shipped binary on a
   > 2-robot fixture that genuinely misses `S ≥ 74p`, and asserts the same
   > failing measurement exits non-zero with `--gate` and zero without it, so
   > the distinction the amendment argues for is held by a test rather than by
   > the discipline of whoever next edits the justfile.
   >
   > It also found a second, smaller instance of the same class: this crate's
   > `[[bin]]` targets all carry `required-features = ["shm"]`, so
   > `cargo nextest run --workspace` skips them whole and `just shm-check`'s
   > `--lib` line does not reach them — **`owner_migration`'s
   > `gate_arithmetic_is_not_vacuous`, which `EVIDENCE.md` cites as the reason
   > that gate's verdict is known to be able to flip, had never been executed
   > by any recipe.** `just shm-check` now runs `--bins`.

   Three things about this measurement, because each was a way of getting it
   wrong:

   - **The `.tft` has to be large, and that is the gate's design rather than a
     convenience.** `(S + 16p)/(S + p) ≤ 1.2` rearranges to `S ≥ 74p`, so with
     any real per-process cost the criterion is only about *sharing* once the
     arena is hundreds of MiB. That is where gate 2's "233 MB index" comes from.
     Run against the 24-frame fixture it would report a failure that is
     arithmetic about process overhead.
   - **Workers must actually read the file.** `open_frozen` is an `mmap`; a
     worker that maps and never touches has no resident share. `--no-touch`
     measures that case and it comes out at **5.32×, FAIL** — with `S ≈ 0` the
     ratio collapses to `16p/p`, so an unread mapping is loud rather than
     silently passing. Every worker sweeps every declared edge across the
     history before reporting.
   - **Pss must be sampled while every worker is alive**, behind a barrier.
     Pss divides a shared page by the number of processes *currently* mapping
     it, so collecting from each worker as it finishes divides an early
     finisher's share by three instead of sixteen. The first version of the
     harness did that and reported **FAIL at 1.43×**. The tell was that the
     solved-for `p` grew with sweep length — 3.4 MiB at 16 stamps/edge to 10.8
     MiB at 4096 — which is not something private memory does. With the barrier,
     `p` is 0.37 MiB at every sweep length.

   > **Amendment — 1.024× is a statement about a *Rust* worker, and the number
   > must be cited with the worker's language and start method attached.**
   >
   > This is an amendment and not a decision record because nothing here is
   > decided. The criterion stands, the harness is right, 1.024× reproduces, and
   > no row is retracted; what changes is the scope of a measured claim, and a
   > qualification that does not travel with the number it qualifies has not been
   > written down anywhere useful. [`0023`](./decisions/0023-the-gate-that-could-not-gate.md)
   > and [`0025`](./decisions/0025-what-build-the-tf2-ratio-gate-speaks-for.md)
   > are records because each *changed* a gate. The moment someone proposes
   > changing this one — a second gated row at a Python worker, or a different
   > fixture — that is a record, and it is named at the end of this amendment as
   > the thing this text deliberately does not do.
   >
   > **The criterion is arithmetic about `p` as much as about sharing**, which the
   > first bullet above already says: `(S + 16p)/(S + p) ≤ 1.2` is `S ≥ 74p`. So
   > the gate's verdict is a function of the worker, and the worker in
   > `frozen_workers.rs` is a Rust process. Every figure here was measured on the
   > development host — AMD EPYC-Milan, 4 physical / 8 logical cores, 31 GiB,
   > Linux 6.8.0-136-generic, numpy 2.5.2, `transform_tree` 0.0.2 — in two
   > sittings, and the second one is why they can now be checked.
   >
   > On **2026-08-17**, on CPython 3.13.12, the Python rows came from a hand
   > harness: 16 workers against **the same `.tft` `just gate4` writes**, each
   > opening it after start, sweeping 64 stamps per edge over `Tree.span` for all
   > 1 536 edges through `Plan.at_into` — the same lookup count the recipe reports
   > — then reporting `Pss` from `/proc/self/smaps_rollup` behind a two-phase
   > barrier, so no worker exits until every worker has sampled. That is the
   > discipline the third bullet above insists on, for the reason it gives.
   >
   > ```
   > $ for w in 1 16; do python pss_run.py target/gate4/workers.tft $w touch; done
   > W=1  touch=touch    total_pss_kib=262826    per_worker=262826.0
   > W=16 touch=touch    total_pss_kib=469219    per_worker=29326.2      -> 1.7853x
   > W=1  touch=touch    total_pss_kib=262777    per_worker=262777.0
   > W=16 touch=touch    total_pss_kib=469233    per_worker=29327.1      -> 1.7856x
   >
   > $ for w in 1 16; do python pss_run.py target/gate4/workers.tft $w notouch; done
   > W=1  touch=notouch  total_pss_kib=25487     per_worker=25487.0
   > W=16 touch=notouch  total_pss_kib=225758    per_worker=14109.9      -> 8.86x
   > ```
   >
   > On **2026-08-19** that harness became a recipe. `just gate4-python` runs
   > `crates/tf_tree_bench/python/gate4_worker.py` under the *same driver* as the
   > Rust arm (`frozen_workers --python`), so the two differ in the worker and in
   > nothing else: same fixture, deleted and re-frozen first; same stamp grid,
   > which is now one constant in `frozen_workers.rs` handed to the Python worker
   > on its command line rather than restated there; same barrier; same
   > `smaps_rollup` read; and the `lookups` column reports both arms answering
   > the same 98 304 queries per worker. Its interpreter is `just py-setup`'s,
   > which is **CPython 3.14.3** — the version the start-method claim below is
   > actually about.
   >
   > ```
   > $ just gate4
   > building 64 robots x 40 s: 1537 frames, 1536 edges, 3225600 samples, 336.0 MiB arena
   > wrote target/gate4/workers.tft — 338.0 MiB on disk (format 3)
   > PHASE5 §12 gate 4 — 16 workers sharing one .tft, total Pss within 1.2x of one
   >   .tft target/gate4/workers.tft (338.0 MiB)
   >   worker  Rust — this binary, re-executed with --worker
   >
   >   workers     total Pss    per worker     lookups
   >         1      229.9 MiB     229.92 MiB       98304
   >        16      235.4 MiB      14.71 MiB     1572864
   >
   >   gate 4, Rust worker: 235.4 MiB / 229.9 MiB = 1.024x against 1.2x — PASS
   >   solving total(N) = S + N*p over the two rows: S = 229.6 MiB shared, p = 0.36 MiB private per worker
   >   the gate needs S >= 74x p, i.e. >= 27 MiB, and S is 230 MiB
   >
   > $ just gate4-python                     # ... same two build lines, then:
   > PHASE5 §12 gate 4 — 16 workers sharing one .tft, total Pss within 1.2x of one
   >   .tft target/gate4/workers.tft (338.0 MiB)
   >   worker  Python — .venv/bin/python crates/tf_tree_bench/python/gate4_worker.py
   >
   >   workers     total Pss    per worker     lookups
   >         1      258.1 MiB     258.07 MiB       98304
   >        16      466.0 MiB      29.13 MiB     1572864
   >
   >   gate 4, Python worker: 466.0 MiB / 258.1 MiB = 1.806x against 1.2x — FAIL
   >   solving total(N) = S + N*p over the two rows: S = 244.2 MiB shared, p = 13.86 MiB private per worker
   >   the gate needs S >= 74x p, i.e. >= 1026 MiB, and S is 244 MiB
   >
   >   This arm REPORTS. Criterion 4 is stated over the Rust worker and its MET is
   >   that row; giving the gate a second arm is a decision and needs a record ...
   >
   > $ ./target/release/frozen_workers --tft target/gate4/workers.tft --workers 1,16 \
   >       --python .venv/bin/python --no-touch    # the control; verdict line only
   >   gate 4, Python worker: 231.6 MiB / 28.5 MiB = 8.120x against 1.2x — FAIL
   > ```
   >
   > **On gate 4's own fixture, gate 4's own criterion fails when the worker is a
   > spawned Python process** — at **1.785×** by hand on CPython 3.13.12, and at
   > **1.804–1.806×** from the recipe on 3.14.3, over repeated runs in two
   > sittings. Solving the two rows gives `S = 243.2 MiB, p = 13.44 MiB` for the
   > first and `S = 244.1–244.6 MiB, p = 13.84–13.86 MiB` for the second — thirty-seven
   > times the Rust worker's `p` either way — so `S ≥ 74p` wants **994** and
   > **~1 025 MiB** of arena where the fixture supplies 338. The no-touch controls
   > fail loudly, at 8.86× and 8.120×, so no Python arm is passing or failing
   > vacuously.
   >
   > The same measurement on a second, smaller fixture (64 edges × 8 192 samples,
   > a 39 MiB `.tft`) separates the start methods:
   >
   > | worker | measured `p` | minimum `S` for `S ≥ 74p` |
   > |---|---|---|
   > | Rust (`frozen_workers.rs`, the gate's own) | **0.36 MiB** | 27 MiB |
   > | forked CPython + numpy | **3.36, 3.37 MiB** | 249 MiB |
   > | spawned CPython + numpy | **14.23, 14.22 MiB** (39 MiB fixture) / **13.44 MiB** (gate fixture) / **13.84–13.86 MiB** across runs (gate fixture, 3.14.3, `just gate4-python`) | 1 053 / 994 / ~1 025 MiB |
   >
   > Two repeats for each of the 2026-08-17 Python cells, agreeing to three
   > significant figures, and repeated runs for the recipe's; the Rust row is one run
   > of `just gate4`, whose spread across runs the **MET** paragraph above
   > records. The 0.8 MiB by which
   > the spawned worker's `p` differs between the two fixtures is consistent with
   > the worker's own output buffer — 8 191 × 4 × 4 × `f64` is 1.05 MB on the
   > small fixture against 8 KB on the gate one — which is an explanation
   > consistent with the numbers, not a second measurement of it. It is also the
   > amendment's point in miniature: `p` is a property of the worker.
   >
   > [`0026`](./decisions/0026-the-corpus-shape-of-a-frozen-index.md) reached
   > the same conclusion from a different corpus and its numbers are close but not
   > identical — Rust 0.37, forked 2.24–2.72, spawned 13.24–13.74 MiB, giving
   > minima of 27.4, 166 and 980 MiB, and a 788 MiB corpus failing at **1.248×**.
   > The spawned and Rust rows agree with the two above; the forked row does not,
   > 2.24 against 3.36, and the difference is left unattributed rather than
   > explained away — the two worker bodies are not the same program, and no
   > measurement here isolates which difference accounts for it. Either number
   > puts the forked minimum in the hundreds of MiB and the spawned minimum near a
   > gigabyte, which is the only thing the gate's arithmetic needs.
   >
   > **Why this matters beyond bookkeeping.** §4.3's amendment and `open_file`'s
   > docstring both say the same thing: a `DataLoader` with `num_workers > 0`
   > sends its dataset to the workers by pickle "under `spawn` *and* under
   > `forkserver` — which is CPython 3.14's default start method on Linux, so
   > this is the common case and not the exotic one". The wedge's audience is
   > therefore the row with the largest `p` in the table, and a torch worker
   > imports far more than numpy, so 13.44 MiB is a **floor** for it and not an
   > estimate.
   >
   > **What is and is not host-bound.** No wall-clock figure appears in this
   > amendment, deliberately: §9.3 forbids presenting one from this box as a gate.
   > Every number above is a Pss byte count or a ratio of two, which is §9.3's
   > `Memory` sensitivity — it fails on a debug build or an unreadable
   > `smaps_rollup` and on nothing else. They are, however, **worker-bound**: `p`
   > is a property of the interpreter, its extension modules and the worker's own
   > allocations, and none of those are properties of `tf_tree`.
   >
   > **Three limits on these numbers. One is closed, one is narrowed, one
   > stands.**
   >
   > *Closed — there is a recipe.* This paragraph used to read "**there is no
   > recipe for the Python rows**: `just gate4` regenerates the Rust row and
   > nothing regenerates these, which is the same shape as a gate row that no
   > workflow invokes", and it ended with the obligation *any record that gives
   > criterion 4 a second worker arm owes a recipe with it*. `just gate4-python`
   > is that recipe. It deletes and re-freezes the fixture the way `just gate4`
   > does, and it **reports rather than gates** — see the two paragraphs below,
   > which it does not touch.
   >
   > *Narrowed — the interpreter.* The 2026-08-17 rows are CPython 3.13 while the
   > start-method claim above is about 3.14; the recipe runs `just py-setup`'s
   > 3.14.3. Between the two, `p` moves 3 % and the ratio 1 % — and the
   > interpreter is one of several differences, not the isolated cause: the
   > worker body is not the same program, and the extension under the recipe is a
   > `--release` build where the earlier run's profile is not recorded.
   >
   > *Standing — no torch `DataLoader` was in the loop.* Both arms are raw
   > `subprocess` (a fresh interpreter, which brackets `spawn`) and raw
   > `os.fork`, which is `0026`'s open question 2 and is unanswered here too.
   >
   > **What this amendment does not do.** It does not change criterion 4, add a
   > row, or move gate 4's **MET**, and neither does the recipe: `gate4-python`
   > exits 0 on the FAIL it prints, and prints why in the same breath. Whether the
   > gate should acquire a second worker arm — and if so which start method it
   > pins, and what corpus size that forces on the fixture — is a decision, and it
   > needs a record.
   >
   > **Since 2026-09-04 that sentence is enforced rather than observed.** When
   > the Rust arm acquired an exit status (the correction under **MET** above),
   > the cheapest possible mistake was to make the `FAIL` global — one `if` with
   > no condition on the arm — which would have turned `just gate4-python` into
   > a failing recipe and *decided* this question in a diff that looked like
   > tidying up. So gating is a `--gate` flag the caller passes, `gate4-python`
   > does not pass it, and the binary refuses `--gate --python` outright with a
   > message naming this paragraph. Measured on the same seeded fixture on
   > 2026-09-04: `just gate4` exited **1** at 5.380× and `just gate4-python`
   > exited **0** at 7.021× (`p` = 13.31 MiB, consistent with the 13.44–13.86
   > recorded above).
   >
   > **What it obliges.** Wherever 1.024× is cited as evidence for the wedge —
   > `README.md`, the §9 benchmark report, a talk — it is cited as *a Rust worker
   > sharing a 338 MiB `.tft`*. Citing it bare, in front of a Python audience,
   > claims a number this host measures at 1.80× on the same file. Both recipes
   > now name their worker in the verdict line they print, so a pasted transcript
   > carries the qualification rather than depending on whoever pastes it.

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
