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
| §1 `FORMAT_VERSION = 3`, Phase 6 **header fields** reserved | **Done, for the header — and the region half of what this row used to claim is retracted.** *The row read* `Phase 6 regions reserved` *until 2026-09-05*, and [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md) measured that no region slot is reserved at all: a Phase 6 spline region is a *twelfth* region, so it needs another `FORMAT_VERSION`. §1.2 carries the retraction and the measurement. What is done: header 256 → 320 with ≥ 64 bytes still reserved (asserted, not intended); the two counter regions; Phase 6's four header fields, declared absent; `nominal_rate_mhz` and `declared_by_slot` in `EdgeRecord`, `frame_kind` in `FrameRecord`; `layout_hash` `0x9075_90F5` → `0x3D10_4195`; `doctor --explain-version`. **Two of this section's own amendments were wrong and are corrected in place.** |
| §2 Frozen arena (`.tft`) | **Done.** `tf_tree_arena::frozen` writes and maps the container; `Tree::open_frozen`/`Tree::freeze_to` and `tf_tree freeze --from-live` are wired. §2.1's bit-for-bit claim is **tested and holds** (`crates/tf_tree/tests/frozen.rs`). Two amendments below: the container header's size, and the one-sided per-edge span. **§2.4's read path is gated since 2026-09-05** — §12 criterion 2, `just gate2` — and §2.4's own listing was corrected in the same change: it named three steps where the code runs five, and omitted the only one that touches a page of the mapping. |
| §3 Bag ingestion | **Partly done — MCAP only.** `tf_tree_ingest` is a new workspace member; §3's opening note rules out `tf_tree_core`/`tf_tree_arena`, and it is not in `tf_tree_cli` because §4's offline Python API needs the same logic and cannot depend on a binary crate. **That consumer exists as of [`0046`](./decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md)** — `tf_tree.ingest_bag` — and until it did, the crate boundary had been drawn and paid for for a caller with no dependency edge to it (`grep -c tf_tree_ingest crates/tf_tree_py/Cargo.toml` was 0). `digest_file` moved to the crate root and its `blake3` dependency stopped being optional so that the binding can report a recording's digest on platforms with no frozen backend; that adds nothing to any build, because `tf_tree_core` already requires blake3. §3.1's two-pass *structure* **including the spill-to-run-file** and §3.3's MCAP source (schema-based discovery, so remapped topics are found) are implemented and gated by `cargo nextest run --workspace`, and so is every §3.2 row **except `--on-clock-reset=split`, which is refused with `IngestError::ClockResetSplitUnsupported` and stays refused on the argument in §3.2's amendment** — the exception belongs in the same clause as the claim, because it was four sentences away from it and the row read as contradicting itself. **The sentence withdrawn here on 2026-09-05 read:** *"§3.1's two-pass structure including the spill-to-run-file, §3.3's MCAP source (schema-based discovery, so remapped topics are found) and every §3.2 row are implemented and gated by `cargo nextest run --workspace`."* It counted the split row, which is a refusal; it counted the static-conflict row, which had shipped as a bare count; and it stood over an unqualified "two passes" while §3.1's pass one does not detect a time domain. It is quoted rather than edited away, because a claim silently repaired stops recording that it was ever stronger. §3.2's static-conflict row says *"report both values"* and the implementation reported a **bare count** — `StaticVerdict::Conflict` carried the two poses and the ingest site discarded them, so the report told an operator that two URDFs disagreed and not which one is installed, which is the whole question §5.7 says the row exists to answer; `Survey::static_conflict_details` now carries one row per contradicted edge (rate-limited by `StaticStore`'s own `first_time`, because `/tf_static` is latched) and both poses reach the JSON and the terminal summary at full `f64` precision. **It was called `static_conflicts` for a day, which is the name of the count beside it** — a `u64` at `anomalies.static_conflicts` and a `Vec` at `static_conflicts`, and in the JSON one key meaning a number or an array depending on nesting depth. And **§3.1's pass one is NORMATIVE that it detects the time domain, and nothing here does** — `rg -n 'TreeBuilder::new\|static_edge\|dynamic_edge' crates/tf_tree_ingest/src/ingest.rs` prints every place this crate builds a tree or an edge and none of them names a domain, so every ingested edge is `SystemDomain` whatever clock the recording used (**this row first cited `rg -ni domain crates/tf_tree_ingest/src/` "returns nothing", which the crate's own doc block falsified by explaining the gap inside the directory being scanned**); §3.1's last amendment is the gap, why closing it in code would be inventing a rule this document does not state, and what would close it. **Something that sentence did not mention at all:** the reader compared a record's declared length against `--max-record-size` **before** it looked at the opcode, so an oversized attachment aborted an ingest whose transforms were all intact — `DEFAULT_MAX_RECORD_BYTES`' own doc comment named that defect and it stood. The opcode decides first now: a record the reader does not read is stepped over and counted as `oversized_records_skipped`, and a `Chunk`, `Schema`, `Channel` or `Message` over the ceiling still refuses, because skipping one of those loses transforms and reads downstream exactly like a recording that never had them. **The first version of that skip promised something it could not check and the promise is withdrawn:** the summary row ended *"were stepped over; no transform was lost"*, while the record is stepped over on the length written in its own header and nothing validates that length — so for every opcode the reader does not read, a ceiling that had incidentally been a corrupt-header detector became a silent resync at whatever offset the length reached. `read_tf` now requires the landing position to look like a record boundary — an assigned opcode whose own length is not larger than the file, the end magic, or the end of the file — and refuses with `RecordTooLarge` otherwise, which is the refusal those opcodes got before any of this existed and names the flag that reads the record properly instead. **What no linear walk can catch is a corrupt length that lands on a *real* boundary**, and `a_skip_that_lands_on_a_later_boundary_loses_transforms_and_says_so` aims one at a later chunk boundary and measures the transforms that vanish between, so the withdrawn sentence is now held down by a test rather than by an argument. The row that replaces it says only what the run knows: it did not look inside that span, and `--max-record-size` is what makes it look. `tf_tree ingest --bag` needs no features; `tf_tree freeze --from-bag` needs `shm` (the frozen backend does) and is gated by `just shm-check`. **Of the four things that were not done, two are closed and two are now decisions with arguments rather than gaps:** §3.1's run file is built — grouping still handles every recording whose largest single edge fits the cap, and one edge over the cap on its own is spilled, *reduced* in bounded passes and merged, so `IngestError::EdgeExceedsMemoryCap` is gone (see §3.1's amendment, including why the reduce pass is not optional); `--on-clock-reset=split` **stays refused with the argument recorded in §3.2's amendment** — it would turn one ingest into N arenas and change every downstream contract, to do worse what cutting the recording already does; §3.3's rosbag2-sqlite3 source **stays absent on a measured dependency finding** (§3.3's amendment: `rusqlite` vendors C, `prsqlite` has no licence and `cargo deny` refuses it, `sqlite-rs`/`sq3_parser` are header parsers) but a `.db3` is now *diagnosed* as one, with the `ros2 bag convert` remedy, instead of being reported as a corrupt MCAP. **`freeze_from_arrays` is still absent**, and it is the one of the four that is a schedule rather than a decision: it needs no dependency and no format change, only a `tf_tree_py` entry point that builds a `Tree` from NumPy arrays, and its gate is `just py-test` / `just py-lint` on two interpreters rather than `cargo nextest`. `Tree.freeze()` — the way *out* — already exists (§4 row). **`--max-memory` bounds pass two's sort buffers and *not* the arena**, at a measured 78 B/sample of arena against the buffers' 64 — the arena is the output and cannot be capped. `ingest::fill` carries the table and `tests/memory.rs` asserts it; an earlier revision claimed "peak memory is the cap either way", which was false. **Since 2026-09-06 the cap also covers the stable sort's own scratch**, which is a sample buffer nothing had counted — see §3.1's amendment for the 1.95× overrun it measured, for what the reserve costs in re-reads, and for why "the arena is the larger of the two" is a statement about the per-sample rates and stops holding on a three-equal-edge fixture in one pass. **§0.0's `default-features = false` on `mcap` costs a measured ~1.8× per pass on a compressed recording and nothing else, and §0.0 carries the amendment:** chunks are taken whole (`emit_chunks`) and decoded by pure-Rust `ruzstd`/`lz4_flex` behind a default-on `compression` feature, so a zstd or lz4 recording — what rosbag2 and Foxglove write by default — ingests transparently with no C build step and `PHASE2.md` §2 untouched. `CompressedChunk` now means a codec outside the specification or a `--no-default-features` build, which is what the `mcap compress --compression none` remedy is for. **The price is measured, not asserted:** `survey` over a 160 000-transform recording takes 0.027 s uncompressed, 0.035 s for lz4 and 0.048 s for zstd, and `ruzstd` decodes libzstd frames at about a quarter of libzstd's own rate — multiplied by a pass count that is `1 + groups + spilled edges`, not a flat two. §12's throughput gate is **measured and gated** since 2026-09-05 (`just gate5`, [`0050`](./decisions/0050-what-ten-times-real-time-divides.md)) and is met by more than an order of magnitude; the three per-codec survey figures in the previous sentence remain what they always were — a hand measurement with no producer, quoted here and in `crate::decompress` — and `just gate5` does **not** re-derive them, because it times a whole `run` on one codec arm rather than a `survey` on three. An earlier revision of this row said the rule "cost nothing", which was the same kind of overclaim as the "peak memory is the cap either way" sentence three clauses up. Decompression is bounded **three** times before anything is allocated: `uncompressed_size` absolutely (64 MiB) and as an expansion ratio (1024×), because neither codec crate bounds total output — and the zstd decoder's *window*, which is a separate field in the codec's own header that neither of those can see. That third bound closed a measured defect: a 26-byte payload of two concatenated zstd frames, with an honest `uncompressed_size` and a correct CRC, decoded to the right answer and drove the allocator to a 134 MiB peak, unaffected by either knob. A truncated *compressed* chunk stays unrecoverable and is reported as truncation, not corruption; zstd is checked against a committed real-libzstd fixture and lz4 against a frame hand-authored from its specification (one frame, not a whole recording — §12 states the remaining asymmetry), and `just ingest-check` — now also run by CI's `test` job — compiles and tests the codec-free configuration that `--workspace` cannot see, plus asserts against the dependency graph that the shipped CLI still links both codecs, which no test inside that crate can. **A truncated recording is read up to the cut** and reported as truncated rather than refused — a SIGKILLed recorder is how bags in the field end. **A review of the spill path found two real defects and they are fixed:** the temporary file's name was derived from the *edge slot*, so two concurrent `fill` calls in one process picked the same path and `truncate(true)` let the second empty the first's inode — silently interleaved samples, no error, and a deterministic collision rather than a race wherever the unlink-at-create cannot run; it is now a process-wide `AtomicU64`. And **the reduce pass's cross-run tie order was asserted by nothing** — two order-inverting edits left the whole workspace green while resolving a duplicate to the wrong pose — so `a_reduce_pass_keeps_the_last_occurrence` now gates it. Two accounting corrections came with them: a reduce pass holds **two** staging buffers and counted one, and the run index (16 B per run) is real memory the cap does not bound and is now reported as `peak_run_index_bytes` rather than omitted. **The *per-run* sort's stability was the one thing this row said was gated by nothing, and it is gated now.** The two call sites are one function, `spill::sort_run`, and `the_per_run_sort_is_stable_so_last_wins_inside_a_run` drives it. The reason it went ungated is worth keeping: the row used to say *"swapping it for `sort_unstable_by_key` survives the whole suite, because a run at the caps these tests use is short enough that `sort_unstable` insertion-sorts and is stable in fact"*, and both existing spill tests run at a 1 KiB cap whose run is fourteen samples. **The first attempt at the new test also survived the mutant**, at a 2 048 B cap whose run is twenty-eight — chosen against the twenty-element insertion-sort threshold, which is not where the standard library's small sort ends for a type this cheap to move. It is written at a 64 KiB cap, four runs of 896, every run entirely duplicate pairs scattered by a coprime stride and no ties across runs, so the per-run sort is the only thing that can decide them. The in-memory sort it mirrors was already gated, by the two tests that compare the paths. **The ingest report's schema tag moved to `tf_tree.ingest/2` on 2026-09-06**, for one key: `undecodable_channels` was the *sum* of the TF-schema channels this build cannot decode and the ones the operator's own `--tf-topic` excluded, so it named one of its two terms and told a consumer pinning the schema that its own narrowing was a defect in the recording. It is `filtered_channels` and `non_cdr_channels` now — different remedies, as `chunks_over_limit` is split out of `bad_chunks` for the same reason — and the field's doc comment was wrong about the unit as well, since these are counted once per channel id rather than once per message. The terminal summary was already right and still prints one row for the pair. **The `filtered_channels` half was exercised by no test in `crates/tf_tree_ingest/tests/`**: nothing there ever built a `TopicRoles` narrowing, so half of the sum was gated by nothing while the whole read as covered. **§11's anomaly corpus exists** — `crates/tf_tree_ingest/tests/anomaly_corpus.rs` carries every reportable §3.2 row in one recording and asserts the **whole JSON document byte for byte**, because `assert_eq!` on one counter is satisfied by the wrong anomaly being counted; the other two rows are refusals that produce no report at all, so they are driven from the same corpus with one message appended rather than from a fixture that happens to be nearby. It is red-tested per row — one seeded violation each. **And what every §3 test reads is a recording this crate wrote**: `tf_tree_ingest::fixture` is the writer on the input side of the reader it gates, so the suite proves this reader's *bookkeeping* and never its agreement with a real `rosbag2` or DDS writer. Nothing in this repository is a rosbag2 bag — `crates/tf_tree_ingest/testdata/ATTRIBUTION.md` opens by saying so, `testdata/rosbag2/synthetic_empty.db3` is a hand-built SQLite file with no message rows, and `/testdata/bags/` is `.gitignore`d. The nearest thing to real data in the tree is `testdata/tfstream/indoor_atelier.tfstream`, a CC BY 4.0 ROS 2 bag's `/tf` converted to this repository's text replay format, which no ingest test reads and which is not an MCAP. `testdata/zstd_conformance.mcap` is the one partial exception and its own attribution states the split: the framing is ours and only each chunk's compressed payload is libzstd's, so it is conformance evidence for the **decoder** and for nothing else. **The amendments below**, in section order: the cap is enforced by two mechanisms and not one, pass one does not detect a time domain, declaration order is canonical, the reset threshold is not the bridge's question, the reset *guard* is per edge, `split` is a decision, and (in §3.3) the rosbag2 sqlite3 source stays absent on a measured dependency finding. |
| §4 Offline Python API | **Done, including all three of §4.4's deltas, and since 2026-08-29 the way *in* from a recording** — `tf_tree.ingest_bag(path)` returns an ordinary `Tree`, and `Tree.source` carries the recording's path and BLAKE3 digest so that `ingest_bag(p).freeze(out)` writes a `.tft` traceable to `p` ([`0046`](./decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md)). **There is no `freeze_bag` beside it and the first draft had one**: `tf_tree_ingest::tft::freeze_bag` is `digest_file` + `run` + `freeze_to`, it streams the *digest* and not the tree, and `Tree::freeze_to` already took `source_digest` — so a second entry point would have been a second spelling of the composition, differing only in whether provenance got filled in, and differing *silently*. `Tree.source` is **dropped by `publisher()`**, because a tree that can be written to may hold samples the recording does not and a wrong digest defeats the one question §2.3 gives the field. Gated by `tests/python/test_ingest.py` against the committed conformance recording, whose invalidation test is a control-and-mutant pair one `publisher()` call apart**, with §4.2 trimmed and §4.3's *reason* corrected — see the two amendments in those sections. `tf_tree.open_file()` returns the ordinary `Tree`, so §4.1's "no parallel offline API" is structural rather than promised; `Tree.freeze()` is the Python way *out*, which is also what makes §4.1's claim testable from Python at all (`tests/python/test_frozen.py` compares live against frozen bit-for-bit through `plan.at`). Of §4.2's five helpers only `span` is API: `resample` is one line of NumPy over `at`, and `edges`/`gaps`/`manifest` need §3's counting pass and a CBOR reader, neither of which exists. **Gated by `just py-test` (CPython 3.14, 200 passed, 2 skipped) and `just py-test-freethreaded` (3.14t, 202 passed — the two skips are the free-threading tests, which only that interpreter can run) — so §4 does *not* inherit Phase 3's 3.14t gap; `uv` fetches the free-threaded build even though the host interpreter is 3.12.3.** **Of §4.4's three API-contract deltas, item 2 — introspection, `tree.frames()` / `tree.edges()` / `plan.edges()` — has landed** (`API.md` §6 row 8), as the *names* half only, which is what §4.4 authorises; the enumeration lives in `tf_tree_py` rather than on `Tree` and the follow-up to consolidate it with `tf_tree_c::unstable`'s and `tf_tree_cli`'s copies is filed in `frames_impl`'s doc comment. **All three refuse a tree inherited across a `fork()`** rather than describing the poison arena `Tree::view` substitutes, and `Tree.instance_uuid` was brought to the same rule — it returned the all-zero value that elsewhere means "in-process". `Tree.__repr__` is the deliberate exception, because a repr that raises breaks the debugger pane a fork victim is reading; it prints `detached-by-fork`. **Items 1 (`Layout::QuatTwist`) and 3 (`from_parts`/`from_timespec`/`from_ros`) are now done too** — `API.md` §6 rows 7 and 9. Item 1 is a keyword-only `layout=` on `at`/`at_into` serving all four layouts, plus an `interp=` on `build`/`open(create=...)`, whose default moved from `"lerpslerp"` to the engine's own `"sclerp"` (`API.md` §3) so that a Python-built tree answers a twist without one; item 3 is `tf_tree.from_parts` and `tf_tree.from_ros` on the Python side and `tft_stamp_from_parts`/`tft_stamp_from_timespec` on the C side (ABI minor 3 → 4), with **one refusal table asserted on both sides of the boundary** — the successes were never the risk. |
| §5 Diagnostic counters | **Done**, §5.6 included — see its amendment: the capture is structural, not a step. Structs and regions landed with §1; §5.4's `Guard` accumulation, the error-path increments and §5.5's default-on `counters` feature are wired. §5.7's measurement is `cargo run --release -p tf_tree_bench --bin counter_cost`: **no measurable contention at or below the CPU count**, so the sharding fallback is not justified. |
| §6 Diagnostics catalogue `TFT001`–`TFT019` | **Partly done.** All nineteen ids exist and are reported (§6's second amendment appends `TFT017` *unclaimed dynamic edge* and `TFT018` *out-of-order stamps*, so **nothing is reported id-less any more**, and its third appends `TFT019` *a wall clock stepped backwards*, which attributes `TFT018`'s evidence rather than detecting anything of its own; the ids are appended and never renumbered, which is what keeps `--suppress` and `--json` compatible); `--json` (schema `tf_tree.doctor/1`), `--exit-code` and `--suppress` are wired. **Seventeen detect** — `TFT001`, `TFT004`–`TFT019` — of which **thirteen run on the reference fixture**: `tf_tree doctor` reports `11 passed, 2 fired, 6 not run` of nineteen — `TFT016` moved from passed to fired when it started reading `transparent_hugepage/shmem_enabled`, the file that governs the live arena's `memfd`, in addition to `transparent_hugepage/enabled`, which does not. Reading only the latter reported this host as healthy while `MappedArena`'s `MADV_HUGEPAGE` was a silent no-op; §2.3's amendment carries the measurement. **Two cannot detect anything in any configuration and say so** rather than passing: `TFT002`/`TFT003`, owned by `tf_tree_bridge::StaticStore`, whose state is process-local. **That sentence is true and one of the two skip *reasons* is not.** `TFT002`'s reason says the state is process-local, which is exactly wrong on `--from-bag`: the ingest runs in `doctor`'s own process, `tf_tree_ingest` counts `Anomalies::static_conflicts` for precisely this condition, and `doctor_source` prints that report to stderr and drops it. `TFT003`'s reason is arena-shaped for the same reason — on a recording the condition is not merely undetected, it is detected and turned into `IngestError::EdgeKindChanged`, which aborts the run before the catalogue executes at all, so *"`TFT003` fired"* and *"the recording ingested"* are mutually exclusive today. Neither is fixed here, and §12 criterion 6 carries what each would need: for `TFT002` a route from the ingest report into `checks::Inputs` **and a decision about the finding's shape** (the count has no edge attached and counts *observations*, so late joiners inflate it — at `error` severity, which moves existing `--from-bag` exit statuses); for `TFT003` a demotion of a hard error to a counted anomaly in `tf_tree_ingest`, which changes what `tf_tree ingest` and `tf_tree freeze --from-bag` produce. Both are decision records rather than commits, and [`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md) puts them out of its own scope without saying whether the recording route is in anyone's. **`TFT004` left that group on 2026-08-27** ([`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md) steps 1–4): `ClaimRecord::clock_offset_nanos` records `wall clock - stamp` per publisher, and the check reads it. **What it detects is narrower than §6's opening asks for, and the amendment there says why** — an offset is clock error *plus* stamp-to-push latency, and one sample cannot separate them, so it fires only past a bound no publish pipeline could account for and reports the fleet spread as a note. The fleet-relative rule needs drift over time, which `tf_tree top` polls for and `doctor` does not have. **The rest skip conditionally**, on evidence rather than on capability — enumerated here and deliberately not counted, because this list, `crates/tf_tree_cli/src/checks.rs`'s header and `crates/tf_tree_cli/src/catalogue.rs`'s header carried three tallies and two answers between them, and the enumeration in `checks.rs` was missing `TFT014` outright; `rg -n 'CheckOutcome::skipped' crates/tf_tree_cli/src/checks.rs` is the instrument for the code: **`TFT004` (four ways — a replayed source, an arena at rest, `TFT005`'s epoch condition, and nothing sampled yet)**, `TFT001`, `TFT018` and `TFT019` (**the push stream, not the arena's liveness** — `TFT001` needs a per-sample writer, which no arena and no recording carries; `TFT018`/`TFT019` need an arrival invariant 6 *rejected*, which only the fixture and a recording carry, since a ring holds none), `TFT005` (the arena's stamps do not share an epoch with the system clock), **`TFT007` (nothing in *this* arena was comparable — no edge declares a nominal rate, or the declaring edges have not retained enough intervals to measure one, or every one of them has **stopped publishing**; the skip reason says which)**, **`TFT008` (it judged nothing: no edge retained enough intervals to measure a spread, or the only ones that did have stopped)**, **`TFT009` (it judged nothing either — both of its halves run only over edges `interval_shape` accepted, and the skip reason names which of that function's three gaps every edge fell into; see §6's amendment, which also records that this id reported `pass` over that empty set beside `TFT008`'s skip over the same one)**, **`TFT010` and `TFT011` (the §5 counters carry no verdict — either the engine was built without `counters`, or *this arena has served no lookups*, which is the amendment below)**, **`TFT013` (three ways — inside the grace period its own row requires; an arena in which nothing has published at all, because bringup and a total outage are the same arena; and an arena whose publishers exist and from which no edge yields a median period — a ring too small to hold two samples, which is permanent at `rate_hz * secs <= 2`, a large ring given only one, or a stream with no positive cadence; the skip reason branches, because a ring-size remedy is false about the last two, and the whole id had been printing the second reason, a sentence false of every one of them. See §6's two amendments: the arena carries no declaration time, so the evidence is how long the longest-running publisher in it has been running)**, **`TFT014` (a frozen `.tft` is a byte copy of the arena, participant records included, so every slot in it names a process that exited when the freeze finished and a file has no assigner for a leaked slot to wedge — see §6's `TFT014` amendment)** and `TFT016` (non-Linux host). **`TFT007` was in the first group and is now in the second** — the amendment in §6 records how: a topology file's `rate_hz` is carried into `EdgeRecord::nominal_rate_mhz`, with no arena field added and no format bump. The reference fixture sizes its rings by slot count, so it still skips *there*, with a reason about that arena rather than about the system. **A `TFT007` `pass` therefore always means at least one edge was compared** — the second skip condition closes a review finding where a declared-but-unmeasurable arena (`doctor` at bringup, or a publisher that has stopped) reported `pass` having compared nothing, with no note either. **What §6's amendment states and does not solve:** `--discover` writes a *measured* rate into the same `rate_hz` the arena reads as an *intended* one, so a recording of a degraded publisher declares the fault as nominal — a discovered rate is a starting point to review. **`doctor` now reads a recording**, which is where `TFT018` and `TFT019` reach a verdict at all: `--from-bag <recording.mcap>` ingests through `tf_tree_ingest::run` and replays the recording's own log order, `--from-file <index.tft>` opens a frozen arena through `Tree::open_frozen`, and both hand the catalogue an ordinary `Tree`. **`TFT018` skips on every *arena* source and not merely on a live one** — §6's amendment records that the live-arena rule was keying on the wrong fact, since a ring holds only the pushes `SampleRing::push` accepted, so a `.tft` would have passed the check unconditionally — and **`TFT019` inherits that skip** rather than working around it, since the evidence is the same one, and skips a second way: when the edges that went backwards are in no wall-clock domain, naming their tags — and the refusal is now *per tag*, so a `SimDomain` edge is sent to [`0012`](./decisions/0012-the-authoritative-clock-jump-signal-and-the-degradation-ladder.md) rather than told its clock cannot have stepped. **§6's "concentrated in a short window" is implemented as at least eight consecutive arrivals invariant 6 would have rejected, and the eight is `tf_tree_cli`'s choice rather than this document's** — §6 names no length, `checks::CLOCK_STEP_MIN_REJECTED_RUN` carries the argument and both of its costs, and it is counted in arrivals because the stamps are the quantity under suspicion and the stream carries no independent arrival clock. Below the threshold the check *passes* and discloses the run length rather than calling a stray inversion an NTP step. Where it attributes some edges and not others, that is disclosed in `Meta.notes`, the same mechanism `TFT007`'s partial coverage uses. **What moved on 2026-09-05.** This sentence read *"Four things moved on 2026-09-05, each with its own §6 amendment"*; the second clause was false — `TFT005`'s record is in §11, which is where a test-plan gap belongs — and the ordinal is not restated, because `git diff main...HEAD -- docs/PHASE5.md \| grep '^+> \*\*Amendment'` is the instrument for what §6 gained. **`TFT013` has the grace period its row has always required** and no arena field was added to get it: the evidence is `(head - 1) x median period` over the arena's own publishers, which is the quantity that does *not* saturate when a ring wraps. **`TFT007` and `TFT008` no longer certify a publisher `TFT009` is calling dead.** They carried `TFT009`'s trailing blindness — every rule in the catalogue measures *between* retained stamps, and a full ring from a publisher that stopped compares equal to its declaration and has a coefficient of variation of ~0 — so `doctor --attach` printed one check calling an edge dead beside checks clearing it — **`TFT008` on any such arena, and `TFT007` as well wherever the edge declared a `rate_hz`**, which is the arena `tf_tree topology --discover` writes and the ROS 2 bridge builds; without a declaration `TFT007` skips outright and the report was one check wide rather than two. The amendment carries the full condition. They **withhold** rather than gaining a finding, because a second warn id for one fault is what the `TFT017`/`TFT018` amendment forbids, and where withholding leaves nothing judged they skip. One predicate (`checks::stopped_publishers`) serves all three — and, since a review found the disclosure had the same hole, `checks::rate_coverage_note` reads it too: it counted a withheld edge as *compared*, so one report could say both *not run, compared nothing* and *compared 1 of 2*. **§6's `TFT008` row is corrected to the rule that shipped** — the inter-arrival spread, not *"p99 ≫ nominal"*; the amendment records which way it moved and why the code was right. **`TFT005` has a test**, recorded in §11 rather than in §6. It had none of any kind, and the reference fixture does not reach it either — the fixture stamps from zero, so `Clock::decide` lands on `NewestStamp` and the check skips there — so neither route had ever executed its firing branch; it now has a unit test pinning `FUTURE_TOLERANCE_NS` at both edges and an end-to-end one on a wall-clock arena. **§11's `doctor --json` row is also met now** and §12 criterion 6 is not — that criterion cannot be met by writing tests, and its own entry says what `TFT002` and `TFT003` would each need. |
| §7 `tf_tree top` | **Done, both halves.** `tf_tree top` exists, attaches read-only and *refuses* `--rw`, and renders all four panes §7 names: per-edge kind/rate/staleness/occupancy/writer, the participant list (arena record ∪ lock-file byte, so read-only participants appear at all), a rolling feed derived from counter deltas, and a per-edge detail view with an inter-arrival histogram. **Built with plain ANSI, not `ratatui` — see the amendment below.** `--web` serves the same [`Sampler`] over a hand-rolled HTTP/1.1 loop on `std::net::TcpListener` (**no new dependency**; a server crate is the third instance of §7's own argument, and the web-view amendment below records it), binding `127.0.0.1:8787` by default. One embedded HTML file, one `/api/tick` JSON endpoint (schema `tf_tree.top/1`), hand-written SVG, **no CDN — enforced by a `default-src 'none'` CSP the server sends, not promised by a comment.** Gated by `cargo nextest run --workspace`: the unit tests in `src/web.rs` plus `crates/tf_tree_cli/tests/web.rs`, which runs the shipped binary and parses the document a browser would receive; `just shm-check` runs the latter again under `--features shm`, which is the build an operator attaches with. **Three defects the amendment names were found by writing those tests and are fixed.** **Not done:** `--web` has no keep-alive, caps itself at 64 concurrent connections and is not a general-purpose server; there is no key handling on either half (see the `ratatui` amendment). |
| §8 Visualization | **Deliberately not built** — this is the finished state, not a gap |
| §9 Benchmark artifact | **Partial.** `just bench-report` emits `report/{results.json,index.html}` with the §9.3 provenance header, every row §9.2's table lists, and all four §9.3 "where we are worse" entries. **No count of the rows appears here, and its absence is the correction:** this row said *"all eight §9.2 rows"* while §9.2's table listed nine and `REQUIRED_ROWS` named ten — three numbers, no two alike, and none of them eight. The tenth (`lookup_ratio_vs_tf2`) had arrived with §9.3's `Ratio` amendment and never reached the table it is required by. §9.2 now carries it, and `report::tests::the_required_row_set_is_the_size_of_phase5_section_9_2s_table` counts that table against `REQUIRED_ROWS.len()`, so the two lists cannot diverge again; **the count belongs to that check and not to this sentence**, which is the same rule this row already learned about keeping a private copy of a measurement. On this host every comparison row is `UNAVAILABLE` with its own reason, which is §9.3's prescribed output, not a gap in the tool. **`Report::validate` reached one of §9.3's five bullets, and this row said it made "the honesty rules structural — the tool refuses to write a report that over-claims".** That was true of bullet 4 and of nothing else: `validate` read `self.rows`, `self.worse` and `self.fitness`, and never `self.provenance` or `self.warmup_discarded_s`, so a report with a **completely empty** provenance header validated cleanly and deleting a `push` line from `Provenance::collect` broke no test. It now reaches **bullets 2, 3, 4 and the enforceable half of 1 and 5** — `REQUIRED_FACTS` as a closed key list, a warm-up that must be positive whenever a row on a timed axis publishes numbers, and a re-derivation command required of every row rather than only of the unavailable ones. **The two halves it cannot reach are stated in `validate`'s own doc comment rather than quietly dropped:** bullet 1's word is *identical* and there is one arm here to compare against nothing, and bullet 5 is a claim about a repository that a running binary cannot see (`every_command_the_report_names_is_a_command_that_exists` resolves each named command against the real `justfile`, which is as close as this file gets). §9.3 carries the amendment; each rule has a test with a seeded violation per arm. **The guard against stale UNAVAILABLE reasons was a three-phrase substring scan, and it caught none of the four reasons that had actually gone stale.** It looked for `"is not implemented"`, `"are not implemented"` and `"unimplemented"` — the phrasings of the two `.tft` reasons that had said §2 and §3 were unimplemented while this table recorded them as landed. A check keyed on wording is defeated by rewording, and four other reasons had rotted without using any of the three: `total_rss_n_consumers` said the tf2 column could not be reached in-process, which the `--features tf2` build does directly and which prints that sentence into `baseline/results-tf2.json`; `publish_to_visible` said no configuration here provides a DDS round trip, false since `ros/tf_tree_bench_ros` and `just dds-bench`; `frozen_row_reason`'s shm branch said this harness builds only synthetic fixtures, false since `src/bin/frozen_workers.rs` freezes ~338 MiB in this same crate; and `tft_16_workers_rss` told a reader to reproduce it on ">= 16 physical cores" for a **`Memory`** row that §9.3's own amendment exempts from the core budget and that `just gate4` measures on four. **All four are corrected, and the decisive half of a reason is now a `Ground` the tool re-derives** from `cfg!(feature = "tf2")`, `cfg!(all(feature = "shm", target_os = "linux"))`, the `Fitness` verdict on the row's own axis, and the measured core count — so a reason that has gone stale is a validation failure rather than a sentence nobody re-read. Three grounds — `MeasuredElsewhere`, `NoInstrument`, `MeasurementRefused` — are **not** decidable there and say so in the type; that is the class this guard still cannot see. **Its first version then refused a report no committed test could produce**: the three N-way rows collected the obstacles that *fired*, so on a host with none the ground list was empty, the new "an `unavailable` row rests on a stated ground" rule rejected the whole report, and `bench_report` would have written nothing on the best host it could run on — invisible here, because this development host has four physical cores and therefore always had an obstacle. The standing gap is that this tool is one process, so it leads the reason unconditionally and each row states a host-independent ground of its own — `MeasuredElsewhere` for the two whose named recipe takes the number, `NoInstrument` for `publish_to_visible`, whose reason says nobody takes it — and a test drives the all-clear host through a handed-in `Fitness`. §9.3 carries that correction and two smaller ones: no row carries its `Ground` into `results.json` (the type's own doc comment claimed a reader could see it there), and the recipe check read `reproduce:` and not `reason:`, while recipes are named in reasons that no `reproduce` field names — `just bench-report`, in both `.tft` rows, is one. **A count of those recipes was written down, in the code and here, and it was wrong; it is not replaced with a corrected count:** differencing the two fields over the emitted artifact answers it, and a copy of that answer in prose is the thing that went stale. **The provenance header recorded the wrong huge-page knob.** §9.3 bullet 3 says "THP setting", singular; `Provenance::collect` read `transparent_hugepage/enabled`, which governs *anonymous* mappings, while a live arena is a `MAP_SHARED` `memfd` governed by `transparent_hugepage/shmem_enabled` — `[madvise]` against `[never]` on this host. `crates/tf_tree_cli/src/hostfacts.rs` exists because `TFT016` made exactly that mistake once. Both keys are recorded now, both are in `REQUIRED_FACTS`, `transparent_hugepage_shmem` joins `runstore::HOST_CRITICAL_FACTS`, and a test reads each key back against the file it claims to come from. **No row became measurable and none was made to measure**: `total_rss_n_consumers`' `tf_tree` column is reachable here but a one-sided memory row is the thumb on the scale §9.3 names, and both `.tft` rows need a build `just bench-report` does not make. **Not done:** §9.1's container image, the public sample recording, `tf_tree bench compare`'s CLI spelling, and §12 gate 7 (reproducing a committed `results.json` on a clean machine). The CLI spelling's blocker is no longer §3 (which landed) but the crate boundary: `tf_tree_bench` is `publish = false` and carries `criterion`, so a shipped `tf_tree` subcommand reaching it would drag a benchmark harness into every install — `CLAUDE.md` routes that to a decision record, not a PR. **§9.1's *measurement* now exists even though its CLI spelling does not** (`just dds-bench`, `ros/tf_tree_bench_ros`): one publisher, real DDS, §5.2's QoS, N `tf2_ros::TransformListener` consumers against the ingest bridge, warm-up discarded and stated, and the whole input set — publisher plan, bridge topology, query set — *generated* from one `tf_tree_bench::workload` entry so §9.3's "identical data" is structural rather than promised. Measured on this host at 4 consumers, in four arms — the two tf_tree ones, the ordinary tf2 deployment, and tf2's *best* case (one composed listener, in the table so the comparison is not a strawman). **The figures live in `docs/benchmarks/tf2.md`'s §9.1 section and are deliberately not restated here.** This row used to carry its own p50 / p99.9 / PSS triple per stack, and not one of those nine numbers survived the run that produced the four-arm table — the same run whose CPU column replaced an instrument that had been reading the main thread's `schedstat` while every arm did its work on other threads. A status table that keeps a private copy of a measurement keeps a copy that goes stale, and this one did. **The arm this row used to say could not exist now runs.** Until [`0015`](./decisions/0015-the-bridge-fills-a-shared-arena.md) landed, `tft_bridge_create` built a *heap* arena with `TreeBuilder::build()` that no second process could attach to, and `dds_report` printed that gap above its own table on every run; this row said the same and called `0015` a **draft**. Both halves are stale: all eight of `0015`'s steps are implemented — it is still **`ready`** rather than `implemented` for the two reasons its own header names, the fork test and §9.2's N = 1…16 curve, and neither of those is this arm — and `just dds-bench` reports **four** arms, the fourth being one bridge process publishing a shared arena under `$TF_TREE_NAME` plus N processes attached to it read-only through `tft_tree_open()`, at 0 % lookup failures. The bridge's CPU and PSS are summed *into* that row and divided by the consumers it serves — it reports `consumers 0` — so the extra process is charged rather than hidden, and the `procs` column shows the N+1. `docs/benchmarks/tf2.md`'s §9.1 section carries the table, the arithmetic and the two places tf_tree is *worse* in it (total PSS at N = 4, and both `.processes` arms' `svc` tails on an unpinned host). That closes §9.2's *total RSS across N consumers* row for a bridge-filled arena and makes §12's criterion 4 demonstrable on the online path, and it leaves this section's remaining gaps the ones listed below rather than the arm itself. **Building it found a real bridge defect**, recorded in `docs/benchmarks/tf2.md`: `tf_tree_bridge::Publisher` is keyed on the resolved *node name* rather than on the GID, so messages arriving before the graph cache resolves claim the edge under an unknown name and `first_writer_wins` then rejects the real publisher permanently — 9 864 of 10 070 transforms dropped and 100 % of lookups failing against a single correctly-declared publisher. Also **not** part of §9 and deliberately not gated: a broader exploratory suite (`just contended-scaling`, `just scale-sweep`, `just soak`, `just bench-run`/`just bench-ab`) covering §11.2's writers-and-pinning row, the width/depth/ring/fan-out axes, multi-minute drift, and A/B comparison of two builds. Those emit `tf_tree.bench-run/1` rather than joining `REQUIRED_ROWS`, because this host fails `Fitness::probe` and a gate that flaps is a gate people learn to ignore. |
| §10 Open-source readiness | **Partial.** Name decision made, measured and recorded ([`0008`](./decisions/0008-the-name-tf-tree.md)): `tf_tree` is free on the crates.io sparse index and is kept there; **PyPI refused it** as too close to the existing `tftree`, so the distribution is `transform_tree` while the module stays `tf_tree` — an earlier revision of this row said the name was free on PyPI too, which `README.md` and `just artifact-versions` both contradict. `LICENSE-MIT`/`LICENSE-APACHE`, `NOTICE`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md` (real address, and an explicit in-scope/out-of-scope boundary around §3.10's trust model) and `SUPPORT.md` (response expectations, platform support, MSRV policy) are in place; `README.md` is rebuilt against the §0.0 tables. **The MSRV was measured and was wrong:** `rust-version = "1.83"` could not build — `blake3` pulls `constant_time_eq 0.4.2`, edition 2024 — so the floor was raised to 1.85 and is now **1.87**, with a CI job that reads it from the manifest and builds `--locked` on exactly it. `just msrv`'s third arm exists because the first two passed while the README said 1.85 and the manifest said 1.87; this row was the same kind of stale copy and is corrected here. Every `publish = false` crate now states its reason in its own manifest. **The benchmark artifact is now a regression gate** (`just bench-check`, CI job `bench-gate`): `crates/tf_tree_bench/baseline/results.json` is committed, and `bench_report --check-baseline` fails on a withdrawn claim, a dropped §9.2 row, a changed `layout_hash`/`format_version`/build profile, or a directional metric past the slack the baseline itself records. **The comparison ignores every host fact by construction** — CPU model, cores, kernel, governor, load and all `reason` prose — because a gate that fails for the CPU model is a gate people learn to ignore; `src/baseline.rs` carries the split. Making that possible needed `results.json` schema `/2`: `/1`'s bare `{value, unit}` gave a consumer no way to know which direction was an improvement. **On this host the gate holds exactly one number** — the LerpSlerp differential's `max_deviation`, the one row that is host-independent by construction — and that is not a placeholder: `Report::validate` now refuses any row that prints numbers while giving none of them a direction, so a row that becomes measurable arrives gated or not at all. **`docs/PHASE2.md` §11.4's `shm_torture` now exists** (`just shm-torture`, 30 minutes, six processes, `SIGKILL` at 6 Hz) with `just shm-torture-asan` for §11.4's "under ASan" and `just shm-torture-self-test` — the seconds-long half that runs in `just shm-check`. It asserts three things, and the second and third are there because the first revision of this harness had none of them and was **vacuous on most seeds**: that an injected corrupt transform is caught by a process that did not write it; that a run validating too little *fails* instead of printing the same `0 violations` a healthy one does; and that `--inject-violation` finishing clean is itself a failure. **The killed processes are joiners, never the rendezvous owner** — see `docs/PHASE2.md` §0.0's §3.5 row for why, and the §11.4 row for what that costs. **The sanitizer rows are wired to recipes that were run on this host, not to a green tick:** `just tsan` (passes), `just shm-torture-asan` (passes on the fixed harness: 152 936 checked reads — 122 344 of them composing all four edges — 477 kills, no ASan report, over 478 observation rounds), and `just cpp-check` for the C++ UBSan half. **There is no Rust UBSan row and its absence is deliberate:** `rustc -Zsanitizer` accepts address/thread/leak/memory and the CFI variants and has no `undefined`, so §10's "ASan/UBSan/TSan" is a C/C++ checklist and its UBSan half lives where there is C++ to check. The nightly workflow (`.github/workflows/nightly.yml`) carries the torture and sanitizer jobs. **Not done, and this list was wrong in two of its three items until 2026-08-29** — its premise, "all three are ceremony **until there is a release**", expired on 2026-08-17, when the project began publishing to crates.io and PyPI. Corrected: **release automation exists and has run.** `.github/workflows/release.yml` publishes the five crates by crates.io Trusted Publisher (OIDC, no stored token) and `wheels.yml` publishes the wheels; both trigger on `v*`, so one tag drives both. **No run tally is written here**: this row carried one — *"has run five times and was green on `v0.0.3` and `v0.0.4`"* — and it was stale within a release, on both halves. A count of workflow runs restales on the next push, and the Actions page is the instrument. **PEP 740 attestations are published** — `wheels.yml`'s `publish` job carries `attestations: write` and `attestations: true` — so naming them here as absent was false; `PHASE3.md` §14's checkbox for them was unticked for the same reason and has been split. **`cargo-dist` is absent; prebuilt binaries are not, and this row's previous argument for skipping them was a non-sequitur.** It read: "`cargo-dist` is absent and is not owed — it builds and uploads binaries for a *binary* release, and the only binary here (`tf_tree_cli`) is `publish = false` by decision." `publish = false` is a statement about the **crates.io index**, and `tf_tree_cli`'s own manifest says its reason is mechanical — three of its dependencies are path-only, so there is no version to publish against — and explicitly *not* a claim about what has landed. Nothing in it bears on whether a user should be handed a binary. The cost of that inference was four tags (`v0.0.1`–`v0.0.4`) shipped to crates.io and PyPI with **no GitHub Release at all** — a tag push creates a tag ref and nothing else — so the audience §4 and the README both put first, somebody holding a recording and no Rust toolchain, had `git clone && cargo install` as the only entry to `doctor --from-bag`. **`release.yml` now builds four Linux rows** (`{x86_64,aarch64}` × `{gnu,musl}`, every one a native runner) through `just release-archive`, and a `github-release` job attaches them with a `SHA256SUMS`. `cargo-dist` itself stays absent on §10's own "or equivalent": it *generates* the workflow from its config and regenerates it on upgrade, and every other job in these workflows carries the argument for its own shape, which a generated file cannot. **Four properties are gated inside the recipe rather than asserted here**: the binary is *executed* and its `--version` compared to the workspace number (a cross-build, a truncated write and a stale artifact all produce a plausible file); the archive is unpacked and re-run through the `tft` symlink; the licence texts travel with it, as Apache-2.0 §4(a) requires and as the crates.io tarballs are already checked for; and packaging is byte-deterministic across mtimes, ownership, **modes** and the gzip header. **Three of those checks were vacuous when first written and are recorded as such in the recipe** — packing twice inside one second cannot see a `date`-derived mtime; gzip zeroes its MTIME field for pipe input whether or not `-n` is passed; and the mode differential did not exist at all, so the builder's umask reached the archive and one commit checksummed two ways on a developer's box against a runner — which is `docs/PROJECT.md` §6's anti-vacuity smell caught in this file rather than after it shipped. `--sort=name` remains untested and says so. **The SBOM generator exists and no release carries an SBOM**, which is a weaker claim than this row made until 2026-09-05 and is the measured one. `scripts/sbom.py` writes CycloneDX 1.5 from `cargo metadata`, and `just sbom` produces the file locally. `release.yml`'s `github-release` job attaches it and covers it with `SHA256SUMS` — **and that step has never executed on any path**: the job is gated on `refs/tags/v*`, the commit that added the step (`d5dd109`, 2026-08-29 23:54 UTC) is not an ancestor of `v0.0.5` (22:39 UTC the same day), and `v0.0.5`'s release assets are the four archives and `SHA256SUMS`. So the wiring is written and unexercised, and the generator had run in no CI job at all. **It now runs in `just lint`**, which is where a script whose only other caller fires during an irreversible release belongs; what that step checks is exit 0, because nothing here can check the document's contents. It is written from `cargo metadata` rather than by adding `cargo-cyclonedx` for the reason this row already gives for not using `cargo-dist` — a generated artifact cannot carry the argument for its own shape. **Its scope is the shipped graph, not the workspace**: walked from the five publishable crates plus `tf_tree_cli` over `normal` edges only, so `criterion`, `proptest` and the rest of the dev graph are absent — **derived by construction; nothing asserts it**, which is the correction to this row's previous *"asserted, not assumed"*. No assertion phrased over the same graph and the same `dep_kinds` rule could fail: the property is a tautology of the function that produces the set. The things that can fail are reached by *running* the generator, which is what the `just lint` step above buys: `cargo metadata`'s `dep_kinds` shape changing, a root acquiring a `normal` edge on something a reader would call dev-only, and — since 2026-09-06 — two REFUSALS the generator did not have. A `ROOTS` name `cargo metadata` cannot resolve used to be dropped silently, so a renamed or removed crate left a walk from the roots that were left, and with every root gone the script wrote a syntactically valid CycloneDX document with an empty `components` array **at exit 0**. It now refuses on both, naming the roots. That is the rule `scripts/artifact-versions.py` states beside `VERSION_FILES` — a scan that silently finds nothing is how a gate keeps passing after the thing it looked at moved — applied where it was missing. Deterministic by construction (no clock, no random UUID; the serial number is derived from the component set), so two releases diff to their dependency change and nothing else. **Signed tags are half-done and the half that is missing is a key, not code**: `release.yml` checks the tag object for a signature and **warns**, becoming a refusal when the repository variable `REQUIRE_SIGNED_TAGS` is `true`. Warning rather than failing is deliberate — a gate that must exist before a key does cannot also block the next release on that key — and all three paths are exercised: a signed annotated tag passes, an unsigned one warns, a lightweight tag (which has no object and can never be signed) warns. `CONTRIBUTING.md`'s *Releasing* section carries the one-time setup. **What remains, and what is declined — a checklist that cannot tell those apart is the defect this row had.** Outstanding: (1) The **mdBook site** — unstarted, with no publishing surface either, and the honest reason is that nobody has asked for it. (2) A **signing key**, which is a maintainer action and not code. (3) §10's **first-five-minutes path**, which is `pip install transform_tree` and is executed by nothing: what `ci.yml` runs on every pull request is `just quickstart`, a from-source `uv` + `maturin develop` build, and `wheels.yml`'s own header says *"Nothing here tests a wheel"*. A quickstart that is executed is not the same claim as *the documented path works*, and this row conflated them. Both halves of §10's one-sentence bullet are [`0052`](./decisions/0052-the-first-five-minutes-nobody-runs.md) (`draft`), which will not let the site be built before the path question is answered — because a chapter that `{{#include}}`s the README's quickstart would publish the clone-first path as the first page. **Declined, not outstanding:** §10's **`license headers`** clause, which this row never mentioned at all — neither done nor declined. It is now **declined with an argument**: [`0051`](./decisions/0051-the-licence-travels-with-the-artifact-not-the-file.md) (`ready`) records that the obligation is on the artifact rather than on the file, that no tracked file carries an SPDX marker and a pass would rewrite every source file in the repository, and that all three distribution surfaces already assert the licence texts travel. And the SBOM's release wiring above stays in this list in spirit: it is written and has never executed. |
| §11 Test plan, §12 Gate | **Partial, and this row is new — §0.0 had rows for §1–§10 and nothing covering the test plan or the gate, which is how §13's box 4 stayed unmentioned by any status row for the life of the section.** §11's **No network** row is **done since 2026-09-04**: `just no-network` traces the five published crates' test binaries under `strace` and asserts every `socket(2)` names `AF_UNIX`, `ci.yml`'s `shm` job runs it on both matrix rows, and §5.1's amendment records the one word of §5.1 it corrects (the suite is the *library's*, not the full one) plus what it does not prove. §12 criterion 4 is **MET at 1.024× and, since the same day, actually gated**: `frozen_workers.rs` printed its verdict and returned `Ok(())` on both branches, so `nightly.yml`'s `gate4` job could not go red — see criterion 4's own correction. Criterion 4's Python arm still **reports** and exits 0, which is its amendment's decision and is now held by a test rather than by whoever edits the justfile next. **Two clauses of this row moved on 2026-09-05 and are recorded rather than overwritten.** *It read* **"criterion 5 (ingest throughput) is held by nobody"**: §12 criterion 5 is **MET and gated** — `just gate5`, [`0050`](./decisions/0050-what-ten-times-real-time-divides.md) — and §13's verdict table says so too, with §0.0's §3 row carrying what that gate does not re-derive. *And it filed* **§11's `--json` schema validation** *under not done* while §0.0's own §6 row already read *"§11's `doctor --json` row is also met now"* — a §0.0 table contradicting itself, on the table CLAUDE.md makes source of truth over the prose that corrects it. **Not done, and each says so where it lives:** criterion 7 (reproduce the artifact from a published container) is §9's gap, and §11's per-check fixtures and `doctor` rows are §6's — §12 criterion 6 is where the two ids that cannot meet that bar in any configuration are named. |

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
  fails on it (5.35–5.62× against ≥ 6×). §9.3 already prescribes the right
  response: omit a row that cannot be measured fairly and say why.
  **The two `.tft` rows do not share a sensitivity, and this bullet said they
  did.** *It read:* "The `.tft` rows — 16-worker total Pss, open time vs bag
  parse — *are* measurable here." `tft_16_workers_rss` is `Sensitivity::Memory`
  and is measurable here, which is §9.3's own amendment retiring the core budget
  for a memory row, and `just gate4` measures it on these four cores.
  `tft_open_vs_bag_parse` is `Sensitivity::AbsoluteTiming` on a host that fails
  the timing axis, **and it is a *comparison* no artifact computes**: `just
  gate2` times the open and `just gate5` times an MCAP ingest, but no recording
  is on both sides — the gated `.tft` is frozen from a generated fleet and was
  never a bag. So the row as a whole was never measurable here, and collapsing
  the two into one clause is exactly the conflation §9.3's `Sensitivity`
  amendment was written to stop. What *is* measurable here, and is measured
  since 2026-09-05, is §12 criterion 2's open time on its own — `just gate2`,
  held outside the report under §9.3's one-sided-budget amendment. The report
  row stays `unavailable` on its missing comparison.
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
| `FORMAT_VERSION = 3` | one deliberate layout break, with room reserved in the **header** for Phase 6 (§1) — not in the region table, which is [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md)'s finding and §1.2's retraction |
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

**That aim was met for the header and missed for the region table, and §1.2 records how.** The reservation this sentence buys is header bytes; a Phase 6 region is a region, and none is reserved. So *once* is now *twice*: [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md) names the second break and gives it a ledger, which is what turns "wait for the next one" into a schedule.

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

Regions whose Phase 6 content does not exist yet are declared in the header with offset `0`, meaning absent.

> **Retracted, 2026-09-05, by [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md) — `ready`.** *The clause continued:* "Phase 6 then fills them **without another layout change**, because the region table already accounts for them." **It is false, and only that second clause is.** The header fields above exist and their offsets are asserted (`crates/tf_tree_arena/src/header.rs`); the **region table** accounts for nothing. `crates/tf_tree_arena/src/layout.rs` declares `N_REGIONS` regions and none of them is a spline region, so a Phase 6 spline region is a *twelfth* region and changes `ArenaLayout::total_size()` for the same declared geometry — i.e. another `FORMAT_VERSION`. `spline_region_off` is a place to write an offset, not a reservation of the bytes it would point at. The break's *size* depends on a Phase 6 nobody has designed; the clause's falsity does not. Do not cite this clause as the reason a byte can wait: `0032` part 2 is the queue a byte joins instead.

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
fstat, pread FROZEN_HEADER_SIZE bytes at 0
validate FrozenHeader (magic, format_version, layout_hash, file_size)
check_extents  (arithmetic on the header's own offsets and lengths)
mmap(arena_off, arena_size, PROT_READ, MAP_PRIVATE | MAP_NORESERVE)
madvise(MADV_HUGEPAGE)                    // best effort
validate_arena_header  (the mapped ArenaHeader: version, layout, magic)
```

**CORRECTION (2026-09-05): this listing was three lines and omitted the last
two.** `check_extents` (`crates/tf_tree_arena/src/frozen.rs`) is pure arithmetic
and cheap, but `validate_arena_header` (same file, via
`crates/tf_tree_arena/src/check.rs`) **dereferences the mapped base pointer** —
so an open touches exactly one page of the arena, where under the old listing it
touched none. That page is the whole of the difference between an open on an
evicted page cache and an open on a resident one, and a harness written from the
old listing would not have known to expect a major fault. It is what §12
criterion 2's evicted arm uses as its own witness: exactly one major fault when
the eviction took, zero when it did not. There is still **no checksum, no name
table walk, no manifest decode and no `source_digest` verification** on this
path — every step is O(1) in the index size, which is the claim §12 criterion 2
actually gates.

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

> **Amendment — the stable sort's own scratch is a sample buffer, and it was in
> neither the budget nor the reported peak (2026-09-06).**
>
> §3.1's sort is stable, because §3.2's "last occurrence in the recording wins"
> is what stability buys. A stable sort allocates: up to one full extra copy of
> the buffer it is sorting. That copy is a *sample* buffer exactly like the ones
> the paragraph above enumerates, and it is live at the peak — `fill` drains its
> buffers from a `BTreeMap` by value, so every buffer it has not reached yet is
> still held while the current one is sorted.
>
> Nothing accounted for it. `plan_groups` packed against `sum(buffers) <= cap`,
> `spill_budget` sized a run against one copy of itself, and the reported
> `peak_buffer_bytes` was computed from `buf.len() * SAMPLE_BYTES` *before* the
> sort ran. Measured with a counting global allocator on `tests/memory.rs`'s own
> fixture: at `--max-memory 1048576` the report read exactly **1 048 576** and
> the process used **2 046 121 B** — **1.95×**. At 4 MiB it was 1.37×, and at the
> 4 GiB default the report understated the run by 1 920 000 B. Attribution was
> not inferred: swapping both `sort_by_key` call sites for `sort_unstable_by_key`
> (which allocates nothing) collapsed every arm to just above its reported
> figure.
>
> **The cap reserves for it now**, and the reserve is the group's **largest
> member** rather than half the cap: the peak of a group is
> `sum(buffers) + max(scratch)`, and the largest scratch is the largest buffer's.
> The spill path's run is sized against `2 × ENCODED`. `FillStats::peak_buffer_bytes`
> includes the term, so the reported number is the one that was enforced.
>
> **The reserve is an upper bound, not a contract.** The standard library does
> not promise what a stable sort allocates; one full copy is the worst case for a
> merge-based one, and today's implementation stays inside it. Reserving the
> bound rather than any measurement of the current heuristic is what keeps the
> cap from depending on it.
>
> **What it costs, stated rather than discovered.** A group holds one edge fewer,
> so a recording whose edges are near the cap gets one more re-read —
> `tests/memory.rs`'s three equal edges went from two fill passes to three. An
> edge between `cap / 2` and `cap` now takes the spill path, because a group of
> one still pays for its own scratch. A spill run is half as long, so an edge
> produces twice as many runs and the reduce loop does more passes —
> `a_tiny_cap_reduces_in_several_passes` went from 175 sorted runs to 347.
> **§12 gate 5's boundary did not move**: its cap is derived from the survey by
> `grouped_cap_from`, which carries the same reserve, so the gated arm still
> takes the two fill passes it declares and `just gate5` still passes. The ratio
> is `just gate5`'s own output; `docs/benchmarks/EVIDENCE.md` is where this
> instrument's readings live and it quotes no number for exactly the reason a
> number here would be wrong.
>
> **The alternative was weighed and is a decision record rather than a patch.** A
> sort that allocates nothing — `sort_unstable_by_key` over an explicit arrival
> index, which recovers "last occurrence wins" by construction — would cost 4-8 B
> per sample instead of 64. It also moves `spill::ENCODED`, a run-file width with
> its own `size_of` test, and it replaces the sort in two places on the path
> §11's byte-identical `.tft` depends on.
>
> **What still has no instrument here**: nothing in this workspace measures an
> allocator. That needs a counting global allocator, i.e. `unsafe impl GlobalAlloc`
> and a row in `scripts/unsafe-budget.txt` for a crate whose library root is
> `#![forbid(unsafe_code)]`. What is gated instead is the arithmetic —
> `ingest::tests::groups_respect_the_cap` asserts `sum + max <= cap` per group and
> `spill::tests::budget_fits_the_cap` asserts `2 × run × ENCODED + staging <= cap`
> — and `tests/memory.rs` says in its own comment that it asserts the report
> rather than the process.

> **Amendment — pass one does not detect a time domain, and this specification
> does not say what it would detect one from.**
>
> The NORMATIVE sentence above lists five things pass one produces: per-edge
> sample counts, `[t_min, t_max]`, frame names, edge kind, **and time domain**.
> `tf_tree_ingest` produces the first four. It produces no domain at all:
> `rg -n 'TreeBuilder::new|static_edge|dynamic_edge' crates/tf_tree_ingest/src/ingest.rs`
> prints where this crate constructs a tree and its edges (`ingest::fill` is its
> only `TreeBuilder` call site) and none of them names a domain, so every ingested
> edge takes `TreeBuilder`'s default, `SystemDomain` (tag `0`), whatever clock the
> recording was made against. **This paragraph first offered `rg -ni domain
> crates/tf_tree_ingest/src/` "returns nothing" as the check, and that command
> stopped returning nothing the moment the crate's own doc block explained the
> gap**: the instrument was inside what it measured. The conclusion is unchanged
> and was independently re-derived; only the procedure was wrong, and its
> replacement is a listing whose output a reader can compare against the file
> rather than an absence a later sentence can break.
>
> **This is recorded as a gap rather than closed in code, because the clause has
> no defined input.** A `tf2_msgs/msg/TFMessage` carries a frame pair, a stamp and
> a pose; the MCAP records around it carry a schema, a topic and a log time. None
> of them names a clock. Every domain anywhere else in this project is
> **declared**, never detected: `tf_tree_bridge::config`'s `domain =` and
> `default_domain =`, `EdgeCfg::domain`, `TreeBuilder::default_domain`. The online
> half does not infer one either — `ros/tf_tree_ros/src/bridge_node.cpp` warns the
> *operator* to configure one when `use_sim_time` is true, which is the same
> admission from the other side.
>
> Two things about a recording *hint* at simulated time and neither is reachable:
> `use_sim_time` is a ROS graph parameter that a bag does not record, and a
> `/clock` topic is a topic name — §3.3 discovers TF channels by **schema** on
> purpose, and says the topic name is consulted for exactly one thing, which is
> not this. Reading a domain off `/clock`'s presence would be a rule this document
> does not state, invented in code.
>
> **What it costs, stated rather than left to be discovered.**
> [`0038`](./decisions/0038-the-domain-a-binding-cannot-name.md) records the
> deployment this exact confusion breaks. An arena whose stamps are simulated but
> tagged as the system clock is indistinguishable, to every consumer, from one
> recorded against a wall clock, and `LookupError::TimeDomainMismatch` — the
> mechanism D9 exists to provide — never fires. A `.tft` built from a sim run and
> one built from the robot compare equal in the single field that separates them,
> and §2.3's `source_digest` does not help: it identifies the recording, not the
> clock the recording was made against.
>
> **What would close it:** a decision naming either the evidence a domain is
> inferred *from*, or — more likely, since it is what every other surface here
> does — the place a **caller declares** it: an `IngestOptions` field, a
> `tf_tree ingest --time-domain` flag, and the same question for
> `freeze --from-bag`. That is new public API on two crates and belongs in
> `docs/decisions/`, not in a pull request.

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

> **Amendment — the assertion exists, and the sentence above needs one word
> changed: the suite is the *library's*, not "the full" one.**
>
> `just no-network` (`scripts/no-network.sh`) is the assertion, and
> `.github/workflows/ci.yml`'s `shm` job runs it on both matrix rows.
> Until 2026-09-04 there was none: every hit of
> `git grep -nE 'AF_UNIX|AF_INET|seccomp|strace' main` was prose or a comment
> rather than an assertion — including `crates/tf_tree_cli/src/web.rs`'s design
> note, which had scoped an assertion that did not exist. **A hit count stood
> here and in two other files and is now deleted rather than corrected again**:
> it was first published wrong, then corrected, and the corrected figure was
> printed beside a basic-regex spelling of that command in which `|` is a
> literal, so a reader who ran what was printed got a different number. The
> qualitative half — every hit prose, none an assertion — was and is correct,
> and it is the half the argument rests on. §13's box 4 had recorded this as
> owed since the section was written, and no §0.0 row covered it, because §0.0 has rows for §1–§10 and
> nothing for §11's test plan. **The word "full" is what moved**, and the
> correction is stated rather than quietly applied: §11's row already said
> *"scoped to the library's suite"* while this sentence said *"the full test
> suite"*, and the two cannot both be satisfied — `tf_tree top --web` binds an
> `AF_INET` listener by construction and is in the full suite. §11's wording is
> the correct one and this paragraph is now read through it.
>
> **What is traced.** The test binaries of the five published crates —
> `tf_tree`, `tf_tree_core`, `tf_tree_math`, `tf_tree_arena`, `tf_tree_ipc` —
> built with `tf_tree/shm,tf_tree_arena/shm`, under `strace -f`, so a spawned
> rendezvous child is followed too. **Every `socket(2)` call observed was
> `AF_UNIX`**, in ~25 s on the development host. The call total is deliberately
> not quoted: it varies run to run on one host, and the script prints its own
> figure on every run. (A single call total stood here and did not reproduce
> across later runs on the same host, which is why it is gone rather than
> re-measured.)
>
> **The exception is a positive control rather than a carve-out**, which is the
> part worth reading. An `AF_INET` socket that the scanner had learned to ignore
> would be invisible; so would a scan pointed at nothing. The recipe therefore
> runs a *second*, separate trace over `crates/tf_tree_cli/tests/web.rs` — a
> package deliberately outside the claim — and **refuses unless it finds
> `AF_INET` there** — the script prints how many it found. The scoping is
> expressed once, as
> a package list, and never as a family the scanner tolerates.
>
> **Three more refusals, because a check that cannot check its property must not
> pass.** No `strace` on the host is a refusal, not a skip. A `strace` that
> cannot see a `socket(2)` it is *shown on purpose* — the script opens a real
> `AF_INET` socket through bash's `/dev/tcp` on every run and requires the
> scanner to catch it — is a refusal, which covers a container without
> `CAP_SYS_PTRACE`, a `ptrace_scope` of 3, and a change in `strace`'s output
> format. A traced binary that exits non-zero is a refusal, because its
> remaining tests never ran. And a run in which `crates/tf_tree/tests/rendezvous.rs`
> was not traced, or opened no `AF_UNIX` socket, is a refusal: that target
> carries `required-features = ["shm"]`, so it is exactly what disappears if the
> feature set drifts. **A bare socket count does not catch that** and was tried
> first — dropping `shm` takes the total to 23, not to 0, because
> `tf_tree_ipc`'s own tests open theirs regardless, and the seeded run passed.
>
> **What it does not establish.** Nothing about a code path no test takes: this
> is a dynamic check over the tests that exist, and an `AF_INET` socket behind an
> unexercised branch is invisible to it. Nothing about a socket the library never
> creates but *inherits* — `tf_tree_ipc` passes the rendezvous fd, and `socket(2)`
> is the syscall this sentence names, not `connect(2)` or `sendto(2)` on a
> received one. Nothing about a non-Linux target. And **nothing about any
> package other than the five traced and the control** — everything else in the
> repository, `crates/tf_tree_tf2_sys` and `xtask` included, is traced by
> nothing. That follows from the scoping above by construction, but it is worth
> saying, because "asserted in CI" over a five-crate subset reads like
> "asserted over the repository" to somebody who has not opened the script.
> (An enumeration of the untraced packages stood here and shipped incomplete;
> the rule replaces it, and `cargo metadata --no-deps` is what generates the
> list.)
> `scripts/no-network.sh` carries the full PROVES / DOES NOT PROVE header, and
> since 2026-09-04 a RED TESTS block naming which refusals have been seeded and
> observed to fire.

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
| `TFT008` | Jitter: inter-arrival spread about the edge's own centre | warn | derived from stamps; **amended below**, this column read *"p99 inter-arrival ≫ nominal"* |
| `TFT009` | Gaps / dropouts, **including the one that has not ended** | warn | derived from stamps; the trailing gap `now - newest` needs a live arena and a wall-comparable clock, and discloses when it could not run |
| `TFT010` | Extrapolation hotspot | warn | `EdgeCounters` + participant attribution (skips on an arena that has served no lookups — see the amendment below) |
| `TFT011` | Ring capacity too small for observed consumer lag | warn | worst extrapolation gap vs buffer span, **or** `capacity × period` vs observed publish latency; skips only when neither has evidence |
| `TFT012` | Disconnected subtree | error | topology walk |
| `TFT013` | Frame declared but never published | info | head == 0 after a grace period |
| `TFT014` | Participant or claim slot leak | warn | Phase 2 lock file vs arena records |
| `TFT015` | Arena occupancy > 80% (frames, edges, participants) | warn | header counters |
| `TFT016` | THP disabled, or `RLIMIT_MEMLOCK` below arena size | info | `/sys`, `/proc/self/limits` |
| `TFT017` | Dynamic edge with no live writer | warn | claim table (added by the amendment below) |
| `TFT018` | Stamps arriving out of monotonic order | error | observed push stream (added by the amendment below) |
| `TFT019` | A wall-clock domain stepped backwards — `TFT018`'s cause, not a publisher fault | warn | `TFT018`'s evidence + the edge's domain tag (added by the amendment below) |

**`TFT016`'s evidence column read `getrlimit` until 2026-09-05 and the code has never called it.** `crates/tf_tree_cli/src/hostfacts.rs` text-parses `/proc/self/limits`, because `tf_tree_cli` is `#![forbid(unsafe_code)]` with no `libc` and its own header gives that reason. **The check's *message* changed in the same pass** ([`0049`](./decisions/0049-the-flag-that-prefaults-the-arena.md)): its detection rule and severity are unchanged, but it no longer predicts that `mlockall` will fail, because it cannot — `mlockall` charges the process's whole address space and this compares a limit against the **arena**. Measured, the call returns `ENOMEM` at limits well above a small arena while this check is silent, so the finding now says outright that its silence is not a clearance, and it names `MCL_ONFAULT` — without which the call prefaults the whole over-provisioned arena.

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
> > than with "unknown tag" — is a refinement it can now make. **That
> > refinement has since landed, and the two sentences claiming otherwise (this
> > one and the closing line of the paragraph below) are corrected in place
> > rather than deleted, so the record still shows what they said.**
> > `checks::tag_refusal` has a dedicated arm for
> > `<SteadyDomain as Domain>::TAG` returning *"a steady clock, which cannot
> > have stepped, so this is a real publisher fault"* — a distinct sentence from
> > the open-trait fallback — rendered into the skip reason through `tag_list`
> > and pinned by a test that asserts the literal string
> > (`checks::tests::tft019_fires_only_on_the_wall_clock_tag_and_names_the_tag_it_refuses`,
> > whose mutant C guards against handing a `SimDomain` edge the steady arm's
> > text). §0.0 never repeated this claim, so the row was clean and the section
> > was not: this is the stale-status failure mode occurring inside a §6
> > amendment rather than in the status table.
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
> > when it was written — **and it is made, as the amendment above records.**
> > `RUNBOOK.md`'s `NonMonotonicStamp` section already describes the shipped
> > behaviour correctly and needs no change.
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
> stream is a replay. §0.0 lists it among the conditional skips. **That sentence read "this is the ninth conditional skip in §0.0" and the ordinal is removed rather than corrected**: it was written before `TFT014`'s own amendment and before `TFT008` and `TFT013` acquired skips, `crates/tf_tree_cli/src/checks.rs` said nine while §0.0 said ten, and an ordinal in prose is a measurement nobody re-takes. §0.0 enumerates; `rg -n 'CheckOutcome::skipped' crates/tf_tree_cli/src/checks.rs` is the instrument for the code.
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

> **Amendment — `TFT008` is the inter-arrival *spread*, not a p99 against the
> nominal, and the table above is corrected to what the code does rather than
> the code changed to match the table.**
>
> The Detection column read *"p99 inter-arrival ≫ nominal"*. What shipped, since
> Phase 1's `inconsistent-rate`, is the **coefficient of variation** of the
> retained inter-arrival intervals about their own mean, above a threshold. Those
> are different tests, and they were left disagreeing for long enough that a
> reader could have built the second one under the same id.
>
> **The move is the row's, and the argument is §6's own, twice over.**
>
> * Judging an interval against the **declared** nominal is what `TFT009`'s doc
>   already refuses for gaps: an edge running at half its nominal has no
>   dropouts, and comparing every interval against the declaration buries the
>   fault this check is for under a rate deviation `TFT007` reports once and
>   correctly. A p99 rule inherits that collision exactly — an edge at half rate
>   fails it on every sample.
> * A nominal exists only where a topology file declared one. `TFT007` skips the
>   whole arena when none does, and that is right for a check whose subject *is*
>   the declaration. Keying `TFT008` on the same evidence would make the one
>   spread rule in the catalogue skip on every arena built without a `rate_hz` —
>   which is the arena a stranger's first `doctor` run is pointed at. A
>   coefficient of variation needs no declaration, and that is the property worth
>   keeping.
>
> **Giving `TFT008` a second statistic under the same id is the thing this
> section's `TFT017`/`TFT018` amendment forbids by name**, so the p99 rule is not
> owed under this id and is not owed under a new one either: nothing has asked
> for a tail statistic that the spread does not already carry, and inventing an
> id for one would be this document deciding a question nobody has raised.
>
> The rule is in `tf_tree_cli::doctor::check_inconsistent_rates` and the
> argument is in `checks::tft008`'s own doc, beside the code, which is where the
> next implementer will be standing.

> **Amendment — `TFT007` and `TFT008` carried `TFT009`'s trailing blindness, and
> they withhold rather than acquiring a second spelling of it.**
>
> `TFT009`'s amendment above added *the gap that has not ended* because every
> rule in the catalogue measures **between** retained stamps, and a publisher
> that stopped three weeks ago leaves a full ring of perfectly spaced samples.
> The other two rules that judge a publisher had the identical hole and were not
> repaired with it: `TFT007`'s observed rate is the median of those same retained
> intervals, so a dead publisher's edge compares **equal** to its declaration and
> reports `pass`; `TFT008`'s coefficient of variation over a flawless ring is
> ~0, which is what a perfect publisher looks like. `doctor --attach` therefore
> printed `TFT009` calling an edge dead and, in the same document, checks
> clearing it — which reads to an operator as two answers against one.
>
> **Which arena prints that pair, exactly, because the sentence above is looser
> than the code.** The report needs a **live** source: `checks::live_wall_now`
> requires `Clock::Wall` *and* `PushStream::RingsUnderWriter` together, so a
> recording and a frozen `.tft` print none of this, correctly. It needs an edge
> whose retained ring supports an `IntervalShape` — monotone, positive median,
> at least four intervals — and has been silent for more than `GAP_FACTOR` x
> that median, which is what makes `TFT009` fire. **`TFT008` is then the second
> answer on any such arena**, since a stopped publisher's ring is evenly spaced
> and its coefficient of variation is ~0. **`TFT007` is the conditional one**: it
> clears that edge only where the topology **declared** a `rate_hz` for it, the
> ring retains more than `RATE_MIN_INTERVALS` intervals, and the observed rate
> falls inside `RATE_TOLERANCE`. Without a declaration `TFT007` skips with *no
> edge in this arena declares a nominal rate* and never reaches the edge at all.
> So *"two checks clearing it"* describes the declared-rate arena — the one
> `tf_tree topology --discover` writes and the ROS 2 bridge builds — and the
> undeclared arena had the same defect one check wide. `checks::tests::a_stopped_publisher_is_not_certified_healthy_by_tft007_and_tft008`
> is the declared case, asserted against the *same* stream at rest and under a
> writer. One more clause, because a status is per check and not per edge:
> "clearing" means the edge is judged and not reported, so on an arena where
> some *other* edge publishes unevenly `TFT008` reads `Fired` and still clears
> this one.
>
> **They are not given a finding, and that is the decision rather than the
> shortcut.** A second warn id reporting one fault inflates the count
> `--exit-code warn` gates on and duplicates a diagnosis, which the
> `TFT017`/`TFT018` amendment argues against by name. Instead each **withholds
> judgement** on such an edge — the shape `TFT007` already used for an edge that
> declared no rate — and where withholding leaves nothing judged, the check
> **skips** with a reason naming the stopped publisher rather than passing on an
> empty set. `TFT008` gains its first skip that way, and it gains a second for
> the case that was already reaching a bare `pass`: an arena where no edge has
> retained enough intervals to measure a spread at all
> (`doctor::SPREAD_MIN_INTERVALS`, which the skip reason quotes rather than
> restating). **That second skip shipped with no test**, which a review found by
> weakening the guard back to the bare `pass` and watching the crate stay green —
> the arm was *executed* by another test that asserts nothing about it, which is
> coverage without a check. `checks::tests::tft008_skips_when_nothing_retained_enough_intervals_to_measure`
> is the test, red against that same weakening and against a smaller
> `SPREAD_MIN_INTERVALS`.
>
> **One predicate, not three.** `checks::stopped_publishers` is the single
> function; `TFT009` reports what it returns and `TFT007`/`TFT008` read the same
> map, so the three cannot come to disagree about which edge stopped. It inherits
> `live_wall_now` unchanged, so it answers nothing on a recording or a frozen
> `.tft`, where `now - newest` is the age of the file. The disclosure is
> `Meta.notes` through `checks::stopped_publisher_note`, the mechanism
> `rate_coverage_note` and `silence_coverage_note` already use.
>
> **The disclosure had the same hole one layer out, and it arrived with the
> repair.** `checks::rate_coverage_note` recomputed its coverage from
> `rate_evidence` alone and never consulted the stopped set, so an edge `TFT007`
> **withheld** was counted by the note as **compared**. A live arena whose only
> declaring edge has stopped therefore produced a report saying both *not run,
> compared nothing* (the skip) and *compared 1 of 2* (the note) — the same
> self-contradiction this amendment exists to remove, reintroduced one layer out
> by the change that removed it. The note takes the clock and the source now and
> reads the same map; the withheld edges are their own term in its sentence
> rather than folded into "too few retained intervals", because the remedy
> differs and because `stopped_publisher_note` names the same edges from the
> other side. `checks::tests::the_rate_coverage_note_and_tft007_agree_about_a_stopped_publisher`
> holds the two to each other, and its first arena is exactly that report.
>
> **`TFT017` is not in this set and is not repaired here.** A publisher that is
> alive and wedged still holds its claim, so *no live writer* is a different
> question from *no recent sample*; making `TFT017` read the sample stream would
> give it `TFT009`'s subject, which is the second meaning this section refuses to
> hand an existing id.

> **Amendment — `TFT009` was the one check in this group with no skip arm, and it
> reported `pass` over an empty subject set beside a `TFT008` skip over the same
> one.**
>
> The amendment above gives `TFT008` a skip *rather than passing on an empty
> set*, and gives it to `TFT008` alone. `TFT009` needed the identical repair and
> did not get it. **Both** of its halves — the retained-gap rule and the
> trailing-silence rule — run only over edges `checks::interval_shape` accepted,
> so on an arena where every edge is declined the finding list is empty and
> `CheckOutcome::ran` renders that as `pass` for a check titled *gaps /
> dropouts* at warn severity.
>
> **It reached further than `TFT008`'s did**, because the floor is higher:
> `GAP_MIN_INTERVALS` + 1 = five retained samples against `SPREAD_MIN_INTERVALS`
> + 1 = four. Two classes are reachable. **Transient**: every arena for its first
> four pushes per edge, which is the bringup run this section says an operator is
> most likely to make, and every publisher restart. **Permanent**: any edge sized
> `RingSize::History { rate_hz, secs }` with `rate_hz * secs <= 4`, since
> `Capacity::history` rounds to a power of two and `SampleRing::retained` is
> `capacity - 1`. On such an arena one document said *TFT008: not run — no edge
> in this arena has retained the 3 intervals an inter-arrival spread needs* and
> *TFT009: pass*, about the identical empty set — and a four-sample stream
> holding a five-second hole at 500x its own cadence passed, while one further
> sample made it fire.
>
> **The reason is three-valued, not two**, because `interval_shape` declines for
> three conditions with opposite remedies: fewer than `GAP_MIN_INTERVALS`
> intervals (wait for the ring to fill, or size it above the floor), any
> **negative** interval (go and read `TFT018`, which fires on the same stream at
> error severity — a gap measured across an inversion is a dropout that never
> happened), and a non-positive median (a publisher stamping one instant). It
> returns a `ShapeGap` rather than a `None` for the reason `RateEvidence` is
> three-valued, and `checks::gap_evidence_skip` states one clause per class
> **present** — never a fixed shape with two zeroes in it. The floor itself is
> quoted from `checks::GAP_MIN_INTERVALS` rather than restated in the message,
> which is what this section already asks of `TFT008`'s reason.
>
> **No verdict moves and nothing new can fire.** `stopped_publishers` reads the
> same `interval_shape`, so the edges it names are a subset of the ones counted
> as judged: where the count is zero the finding list is provably empty, which a
> `debug_assert` in that arm holds rather than a comment. What changes is `pass`
> → `not run` on sparse arenas, so a `--json` consumer sees `not_run` where it
> saw `pass` and the human report's `N passed / M not run` line moves.
> `Report::has_error` and `is_healthy` count **findings** and never statuses, so
> no process exit status changes. The reference fixture is unaffected — its
> dynamic edges retain far more than five samples — and `tf_tree doctor` still
> reports `11 passed, 2 fired, 6 not run`.
>
> `checks::tests::tft009_skips_when_no_edge_retained_enough_intervals_to_measure_a_gap`
> is the test, red against removing the arm, against `GAP_MIN_INTERVALS = 3`, and
> against dropping the empty-arena sentence;
> `checks::tests::an_out_of_order_stream_is_not_reported_as_a_dropout` is red
> against folding the reordering class into the too-few one, and it asserted
> `pass` on an empty subject set until this amendment.
>
> **The disclosure in `Meta.notes` had to move with the skip, and it is a
> different mechanism from the one two amendments up.** `checks::silence_coverage_note`
> discloses that the *trailing-silence* half could not run on a source nobody is
> writing, or on stamps sharing no epoch with the system clock. Beside a
> `not run` line for the same id, that sentence claims the retained-gap half did
> work it did not do — one report saying *TFT009: not run — nothing to measure a
> trailing silence against* and, in its notes, *TFT009 measured gaps between
> retained samples but not the gap since the newest one*. **Neither predicate
> could catch the other**: the skip is about the arena's samples and the note is
> about the clock and the source, and they are independent, so a second
> derivation would be a third thing to keep in step. The note reads the check's
> **outcome** instead and returns `None` when it skipped, which is the rule
> `TFT011`'s two disclosures already follow — when both halves are blind the
> check skips and its reason carries both sentences, rather than the report
> explaining itself twice.

> **Amendment — `TFT013` has the grace period its own row has always required,
> and the evidence for one is the arena's publishing rather than a new field.**
>
> The row above reads *"head == 0 **after a grace period**"*. The predicate was
> `kind == Dynamic && head == 0` with no time term at all, so `doctor` run at
> bringup — before the publishers start, which is the run an operator is most
> likely to make — reported every dynamic edge in the arena. Severity is `info`,
> which is the only reason this was survivable.
>
> **There is no declaration timestamp in the arena and none is added.** `CLAUDE.md`
> forbids adding arena fields opportunistically and
> [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md) is
> the queue such a field would join. The grace period is measured instead against
> the only clock the arena keeps about itself: **how long the longest-running
> publisher in it has been running**, which is `(head - 1) x median period`.
> `EdgeRecord::head` is the monotone total of accepted pushes (invariant 5) and
> not a ring index, so that quantity keeps growing across every lap the ring has
> wrapped — the *retained* span, which is the obvious quantity, saturates at
> `capacity / rate` and would leave a 100 Hz edge with a 100-slot ring reporting
> one second forever, so an arena that had been up for a week would never clear
> the grace. Both terms are lower bounds, which is the conservative direction:
> the check accuses late rather than early.
>
> **The *longest*-running, and only a **dynamic** publisher** — both clauses are
> load-bearing and neither was pinned by the tests that shipped with them, since
> every fixture had a single publishing edge, where a maximum and a minimum are
> the same number. A minimum would let one restarting publisher reset the grace
> for the whole arena, so an edge nothing has ever published to would go
> unreported while any publisher is young; a static edge's stream — which a
> correct arena does not have, but a hand-built or corrupt record does — must
> not be what clears it, for the reason `rate_evidence` refuses a static edge's
> stray nominal rate. `checks::tests::the_grace_period_reads_the_longest_running_dynamic_publisher`
> holds both, running its pair of publishers in both orders so that neither
> "the first that measures" nor "the last" survives either.
>
> **The length is `tf_tree_cli`'s choice rather than this document's**, exactly
> as `CLOCK_STEP_MIN_REJECTED_RUN` is — §6 names none. The value is written
> once, in `checks::DECLARATION_GRACE_NS`, which carries the argument and both
> of its costs; a copy of it in this paragraph is a number that can drift from
> the constant the skip reason prints.
>
> **An arena in which nothing has published at all is a second, separate skip.**
> It is a robot at bringup and a robot whose every publisher has died, and no
> fact in the arena separates them; `TFT017` is the id that reports the second.
> Choosing either reading would be the fabricated answer this section spends its
> length refusing, in the one state where both readings are common.
>
> §0.0's conditional-skip list gains `TFT008` and `TFT013` by these two
> amendments.

> **Amendment — the grace evidence is *unobtainable* on some arenas, which is a
> third skip and not the second one, and the sentence above was printed to
> operators it was false about.**
>
> The amendment above says the second skip is *an arena in which nothing has
> published at all*. **That is not the condition the code tested.** The
> predicate was *no dynamic edge yields a median period*, and
> `doctor::median_period` needs **two** retained samples. `Capacity::history` is
> `next_pow2(ceil(rate_hz * secs))` and `SampleRing::retained` is
> `capacity - 1`, so an edge declared `rate_hz * secs <= 2` retains one sample
> for the life of the arena. A topology declaring `rate_hz: 1.0,
> history_secs: 2.0` — a slow edge kept briefly, which is an ordinary thing to
> write — therefore made `TFT013` skip **permanently**, telling the operator
> *nothing in this arena has published a measurable stream … an edge with
> `head == 0` is what every dynamic edge of a correct arena reads as at bringup.
> TFT017 is the id for an edge whose writer is gone* about an arena whose
> publisher had accepted 3 600 pushes. The boundary is sharp:
> `history(1.0, 3.0)` rounds to four slots, retains three, and the same arena
> fires.
>
> **The fact that separates them was already in hand.** `checks::publish_activity`
> reads `EdgeRecord::head` three lines before it asks for a median, so *some
> publisher exists* and *no publisher can be measured* were both available and
> one `None` collapsed them. It returns a three-valued `PublishActivity`
> — `NoPublisher`, `Unmeasurable { .. }`, `Running(ns)` — for the
> reason `RateEvidence` is three-valued two rows up: a new absence cannot compile
> without a sentence. **One walk, not two**: an "is anything publishing" pass
> beside a "how long" pass is the shape `rate_coverage_note` was repaired for,
> where two derivations of one coverage number disagreed inside one report.
>
> The second reason names the obstacle **this** arena has, and that is not one
> obstacle. `doctor::median_period` declines a stream shorter than two samples
> and a non-positive median alike, so *no dynamic edge yields a median period*
> covers three arenas: a ring that cannot hold two, a large ring that has only
> been given one — every `doctor --attach` at bringup, and a `--from-bag` run
> whose recording carries one dated record for an edge — and a publisher
> stamping one instant into a full ring. A ring-size remedy is false about the
> last two, which is the sentence-false-about-the-arena defect this whole
> amendment is about, one level down. So `Unmeasurable` carries the largest
> ring's retained capacity and the largest number of samples actually recovered,
> and the reason branches on them: each branch names only what its own arena can
> act on, and the third sends the reader to `TFT009` and `TFT018`, which own the
> cadence gaps. **Whether the grace can be *cleared* in that state is not
> decided here**: `(head - 1) / nominal_rate_mhz` would do it where a rate is
> declared, and that substitutes a **declared** rate for a **measured** one,
> which is a §6 amendment and not a patch — it would start an `info` check
> firing on arenas silent since the grace period landed.
>
> `crates/tf_tree_cli/tests/catalogue.rs::tft013_skips_with_the_ring_size_reason_on_an_arena_whose_publisher_it_cannot_measure`
> is the test, and it is an **integration** test on purpose: the claim is
> reachability, and a hand-built `Snapshot` given `head = 3600` and one
> observation by fiat proves the reason and not that any arena is ever in that
> state. It drives the first two arenas through a real `TreeBuilder`;
> `checks::tests::tft013_names_which_of_the_three_unmeasurable_arenas_this_is`
> reads all three arms of the message, including the one that needs stamps a
> `TreeBuilder` will not accept.

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
| **Depth-3 hot lookup, `tf_tree` vs `tf2` (paired ratio)** | median per-round quotient, floored at 2.0× |

**The tenth row was added to this table on 2026-09-04, and it had been required by the tool for longer than that.** `lookup_ratio_vs_tf2` arrived with §9.3's `Ratio` amendment below, went straight into `crates/tf_tree_bench/src/report.rs`'s `REQUIRED_ROWS`, and never reached the list it is required by — so §9.2's table said nine, `REQUIRED_ROWS` said ten, and §0.0's row for §9 said *"all eight §9.2 rows"*. Three counts, no two alike, and none of them eight. **The set is now one list held by one check**: `report::tests::the_required_row_set_is_the_size_of_phase5_section_9_2s_table` counts the rows of this table and compares them to `REQUIRED_ROWS.len()`, so adding a row to either side alone is a test failure. That check *counts*; it does not match names, because this table's cells are prose titles and the constant's entries are ids — a row renamed on both sides passes it, and the drift it is written against is a row added on one side only.

The `Plan::at` row is `tf_tree` against itself and belongs in this table anyway: it is the only measurement of the path an **embedder** actually compiles. `PHASE4.md` §7 gates the C ABI at 5% against native in-crate Rust, and nothing gates native *out-of-crate* Rust, which is what a user's node links. [`API.md`](./API.md) §2.3 makes the row and the gate normative, along with the `#[inline]` attributes and the LTO guidance that are how it is passed. Report it with the embedder's default profile, **not** this workspace's — `[profile.release]` here sets `lto = "thin"`, which is precisely what hides the effect.

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

> **CORRECTION (2026-09-06) — the paragraph above is a reading taken while the row still had an independent variable, and it is not this tree's state.** Since 2026-08-29 `Plan::at_tagged` has sat between `Plan::at` and the fold carrying no `#[inline]`, so **both** of row 1's columns compile to the same call stub around one out-of-line symbol in `tf_tree_core` — `nm -C --print-size` gives them the same body size and `objdump -R` resolves both `call`s to the same slot. Which crate the *stub* was compiled in changes nothing: the quotient is 1.0 by construction, the row prints a clean `within`, and `Verdict::Over` is unreachable. So the criterion is neither met nor unmet here; it is **unmeasured**, and the figures above say nothing about what the boundary costs today in either direction. `just embed-cost` now diagnoses the collapse before it times anything, and refuses unless `EMBED_COST_KNOWN_COLLAPSED=1` is set. [`API.md`](./API.md) §2.3's 2026-09-06 amendment carries the run, the marked-`at_tagged` toggle and the trade that restoring the variable is; it also records that the toggle's own probe bodies consume one pose component, which is a shape `Plan::at_tagged`'s doc names as sign-inverting. **This account is spelled in several files. The census is `grep -rn '1\.25' docs crates`, and no count of it is kept here.**

### 9.3 Honesty requirements — NORMATIVE

The first skeptical reader will look for a thumb on the scale, and finding one ends the project's credibility permanently.

- Identical QoS, identical executor configuration, identical DDS vendor and version, all recorded in the report.
- Both stacks warmed; discard the first N seconds; state N.
- Report `tf2` version, ROS distro, RMW implementation, kernel, CPU model, and THP setting.
- **Report where `tf_tree` is worse**, in the same table and not in a footnote: arena memory floor (an idle arena costs more than an idle `tf2` buffer), attach latency, the operational cost of a format bump, and the bridge as an additional process to supervise.
- Publish the harness source in the same repository. No private benchmark.

If a row cannot be measured fairly, omit it and say why. An honest gap is worth more than a favourable number nobody trusts.

**Amendment — "THP setting" is two knobs, and the report recorded the one that does not govern a `tf_tree` arena.** Bullet 3 says *THP setting*, singular. Linux has two: `/sys/kernel/mm/transparent_hugepage/enabled` governs **anonymous** mappings, and `/sys/kernel/mm/transparent_hugepage/shmem_enabled` governs **shmem** ones — which is what a live arena is, a sealed `memfd` mapped `MAP_SHARED`. They disagree by default and they disagree on this project's development host: `enabled` reads `always [madvise] never` while `shmem_enabled` reads `always within_size advise [never] deny force`. `Provenance::collect` recorded only the first, so the report's THP fact said `madvise` while §2.3's 2 MiB alignment bought the arena nothing. **This repository had already paid for that mistake once**: `crates/tf_tree_cli/src/hostfacts.rs` exists in its present shape because `TFT016` reported a host as healthy off `enabled` alone while `MADV_HUGEPAGE` on the arena's mapping was a silent no-op, and its `ShmemThp` type carries the argument in full. The report now records **both**, under `transparent_hugepage` and `transparent_hugepage_shmem`, each named for its sysfs file rather than collapsed into a verdict — because which one governs depends on the row, and the frozen `.tft` path is a *file* mapping governed by neither. `Report::validate` requires both to be present (`REQUIRED_FACTS`), and a test reads each key back against the file it claims to come from, so wiring the two keys to one file fails rather than passing quietly.

**Amendment — `Report::validate` reaches bullets 2, 3, 4, and half of 1 and 5; it used to reach only bullet 4.** §0.0's row for §9 said this method *"makes the honesty rules structural — the tool refuses to write a report that over-claims"*, and for its whole life `validate` read `self.rows`, `self.worse` and `self.fitness` and never `self.provenance` or `self.warmup_discarded_s`. Four of the five bullets above were prose. What is enforced now: bullet 3's six facts and bullet 1's three are a closed list (`REQUIRED_FACTS`), each present and non-empty, so deleting a `push` line from `Provenance::collect` is a test failure rather than a silently thinner header; bullet 2's warm-up must be a finite non-negative number, and must be **positive** whenever a row on the `AbsoluteTiming` or `Ratio` axis publishes numbers — `Memory` and `HostIndependent` rows are exempt, for the same reason §9.3's `Sensitivity` amendment exempts them from the timing checks; bullet 5's mechanical half is that **every** row names a command that re-derives it, not only the unavailable ones. **Two limits are stated rather than papered over.** Bullet 1's word is *identical* — the same QoS and executor on both stacks — and this report is assembled in one process that stands up neither, so there is one value per key and nothing to compare it against; when the N-way rows acquire a two-arm harness, the comparison is theirs. And bullet 5 is a claim about a repository, which a running binary cannot see: `validate` checks that a command is named, and `every_command_the_report_names_is_a_command_that_exists` resolves each one against the real `justfile` and the real target files, and neither says the harness is public.

**Amendment — an unavailable row's reason now rests on a machine-checked `Ground`, because a guard keyed on wording is defeated by rewording.** §9.3's *"say why"* was held by free prose plus one test scanning for three phrases — `"is not implemented"`, `"are not implemented"`, `"unimplemented"` — which were the phrases the two reasons that had gone stale happened to use. Four further reasons in the same file had gone stale without using any of them: `total_rss_n_consumers` said the tf2 column could not be reached in-process, which is false in the `--features tf2` build that times `tf2::BufferCore` directly and which prints that sentence into `baseline/results-tf2.json`; `publish_to_visible` said no configuration here provides a DDS round trip, false since `ros/tf_tree_bench_ros` and `just dds-bench`; `frozen_row_reason`'s shm branch said this harness builds only synthetic fixtures, false since `src/bin/frozen_workers.rs` in the same crate freezes ~338 MiB; and `tft_16_workers_rss` told a reader to find sixteen physical cores for a **`Memory`** row that this section's own amendment exempts from the core budget, and that `just gate4` measures on four. The decisive half of a reason is therefore an enum value — `Ground` — that `Report::validate` re-derives on every run from `cfg!(feature = "tf2")`, `cfg!(all(feature = "shm", target_os = "linux"))`, the `Fitness` verdict on the row's own axis, and the measured core count. The prose stays, as elaboration on a claim the tool checks. **Three of the seven grounds are undecidable here and are named as such** — `MeasuredElsewhere`, `NoInstrument` and `MeasurementRefused` are claims about the repository and about this run, not about the build or the host. The first is partly covered, in that the recipe it names is resolved against the `justfile`; the last two are as trustworthy as whoever wrote them, which is why they are separate greppable variants rather than folded into the others.

**Correction (2026-09-04) — the first version of that mechanism refused the whole report on the best host it could run on.** `assemble` built the grounds under the three N-way rows (`cpu_per_consumer`, `publish_to_visible`, `scaling_curve`) by pushing only the obstacles that *fired*: no ROS, too few physical cores, a host that failed the timing probe. On a host with none of the three — ROS installed, at least `consumers + 1` physical cores, quiet, non-SMT, `performance` governor — the list came out **empty**, the rows were still `unavailable`, their reason read *"no obstacle was found for a {n}-consumer comparison on this host"*, and the rule added in the same change ("an `unavailable` row rests on a stated ground") rejected the report, so `bench_report` would have written nothing at all. No committed test could see it: `assemble` probes, and this repository's development host has four physical cores, so `core_reason` was always `Some`. The defect is one the rule was written to catch, arriving as an *absence* rather than as a false sentence. Two things are fixed. The reason now **leads** with the gap that is permanent — this tool is one process, it stands up no consumers and links no second engine, so no configuration of it measures an N-way cross-engine row on any host — and the host obstacles are appended to that, conditionally, as before. And each of the three rows now states a ground that does not depend on the host — but **not the same ground, and that distinction is the second half of the fix**. `cpu_per_consumer` and `scaling_curve` say `MeasuredElsewhere`, which is honest because the recipe each names (`just mp-bench-tf2`, `just tf2-scaling`) really does take the number. `publish_to_visible` says `NoInstrument`, which it already said unconditionally, because its own reason states that nothing in this repository times publish-to-visible end to end — seeding `MeasuredElsewhere` into the shared list would have published exactly the over-claim the `Ground` type exists to stop, on the one row whose reason denies it. `report::tests::a_host_with_no_obstacle_still_grounds_every_n_way_row` drives the all-clear host through a `Fitness` handed in rather than probed, and asserts all three halves: each row carries its own standing ground, `publish_to_visible` carries *not* the other two's, and the report validates.

**Correction (2026-09-04) — the `Ground` is not in `results.json`, and its own doc comment said it was.** `Ground::as_str` was documented as *"the JSON/HTML spelling, so a reader of `results.json` sees the ground and not only the prose"*, in the same change that decided not to emit it: `to_json` writes the schema `SCHEMA` names, and a new key there is a consumer-visible change that rides a schema bump. `grep -c ground target/bench-report/results.json` answers 0. The spelling is used by `Report::validate`'s refusal messages and by the test that seeds a stale ground; the doc says so now. A reader of the artifact sees the prose reason and not the ground it rests on — which is a real limit of the published half, recorded here rather than papered over.

**Correction (2026-09-04) — the recipe check read `reproduce:` and not `reason:`.** `every_command_the_report_names_is_a_command_that_exists` is offered above as the one partial mitigation for `MeasuredElsewhere`, and it scanned each row's `reproduce` field only, while recipes are named in reasons that no `reproduce` field names — `just bench-report`, which `frozen_row_reason` puts in both `.tft` rows' reason, is one. It reads both now. Its first version of that scan matched **nothing**, because a `reproduce:` field spells a command bare and a reason spells it in backticks, so the leading token is `` `just `` — the anti-vacuity floor was low enough to hide that, and it is raised. **The counts that stood in this paragraph are deleted rather than corrected:** a list of the recipes named only in reasons, which was closed and did not contain `just bench-report`, and the before/after token counts the raised floor sits between. The floor is in the test and the counts are not: re-derive them by replacing it with an unreachable value and reading the panic message.

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

**The `Ratio` axis has its first row, and it is the tf2 comparison.** `lookup_ratio_vs_tf2` times a depth-3 hot lookup on both engines in one process, `LerpSlerp` on both sides, the arms interleaved within every round with the leading arm alternating, and reports the **median per-round quotient** rather than the quotient of two medians. Measured on the development host: **2.47× with a band of 2.457–2.532 (3.0% wide)** — on the same host, in the same run, whose absolute latencies are `unavailable` because the fitness probe fails. **That band is one draw, not this statistic's typical width**: [`0025`](./decisions/0025-what-build-the-tf2-ratio-gate-speaks-for.md) measured the same row three times on the same pinned harness and records the workspace arm at 1.3–16.7% wide, so ~3% sits low in that range rather than being what a re-run should be expected to reproduce. That contrast is the justification for the axis existing.

Two things about it are stated in the row's own note and repeated here because they bound the claim. The tf2 column goes through `tf_tree_tf2_sys` and therefore **flatters `tf_tree`** by the residual FFI boundary, **45.3 ns / 10% at this depth** — 498.2 ns through the binding against 452.9 ns native, a subtraction between two rows of `docs/benchmarks/tf2.md`'s own bracket table, under that document's *Where the 45.3 ns comes from* paragraph. *This line read `~21 ns / 8%` until 2026-09-05*, a figure that document withdrew for having no derivation recorded anywhere and for disagreeing with its bracket table by a factor of two. Correcting it does not move the gate: `FLOOR` is bounded by the unpaired native-against-native estimate, which has no binding in either half. The binding-free comparison is `docker/tf2/native_scaling.cpp` and its headline is 2.7×. And `ns_per_lookup` on either side is reported but **never gated** — it is an absolute duration, and this host cannot claim one.

**There are two committed baselines**, `results.json` and `results-tf2.json`, checked by `just bench-check` and `just tf2-bench-check`. They are not interchangeable: the status comparison is one-directional, so a single baseline cut with `--features tf2` would make the default recipe fail on every host without ROS 2 — on the difference between two recipes rather than on the code, which is the trap `bench-check` already documents for `--embed-cost`. Each recipe checks the baseline cut by the matching build, and `bench_report` names the right regeneration recipe in its failure message.

**Both committed baselines carry prose that the current binary no longer emits, and `baseline::compare` structurally cannot see it.** The comparison reads schema, provenance rules, row ids, the `where_we_are_worse` set and the metric values — never a row's `reason` or `note` — which is the same deliberate blindness that keeps a CPU model out of the gate. The consequence is that the artifact's *explanatory* half drifts silently: `total_rss_n_consumers`'s `reason` in both files still says the tf2 column needs a ROS 2 install, a sentence `report.rs` replaced, and the ratio row's `note` in both files still prices the FFI boundary at the withdrawn `~21 ns / 8%` corrected above. **The repair is not `just bench-baseline-update`**: regenerating makes the prose current by rewriting every number in the file, which launders a measurement change through a documentation fix. What is owed is either a check that a baseline's `reason`/`note` strings are still producible by the build that reads it — and such a check has to assert a non-zero row count first, or it passes on a report that emitted nothing — or a deliberate regeneration whose diff is the commit's subject. Neither is done here.

The JSON keeps its `timing_sensitive` field with its original meaning (*this row reports an absolute duration*), so `tf_tree.bench-report/2` does not change shape. Each rule carries a test with a verified mutant in `report.rs`.

**Amendment — a one-sided BUDGET with a stated margin may be gated on a host that fails the timing probe, and §12 criterion 2 is the case that forced the question.**

The four `Sensitivity` axes above answer *can this host produce this number*, and they were written for `bench_report`'s rows — every one of which is compared against a **committed baseline**. That is a two-sided question: being 5% slower and being 5% faster are both findings, so any host effect in either direction invalidates it, and `AbsoluteTiming` therefore refuses on every check.

§12 criterion 2 is not that shape. It is a **budget** — *under 10 ms* — and the direction of the error is what decides whether an unfit host can publish it. Every timing check `Fitness::probe` fails on this development host makes a measured duration **longer**, never shorter: SMT contention, a busy machine, an unreadable governor. So on such a host

* a **PASS with margin is a conservative claim** — the host can only have inflated the number that fits;
* a **FAIL is not attributable to the code**, and must not be read as one.

That is the whole of the licence, and it buys exactly one of the two directions. Three things follow, and a gate taking it owes all three:

1. **State the margin, per run, from the measurement rather than from prose.** §12 criterion 2 prints its worst reading against the budget on every run, and prints the fitness verdict and its reasons beside it whichever way the verdict goes.
2. **Gate the arm that is a claim about the code.** For an `mmap` the *evicted*-page-cache arm's size dependence is the storage device fetching pages — measured, the major-fault count of an open does not move between a 2 MiB and a 338 MiB index — so a gate on it is a gate on the disk under the runner. It is reported with the host beside it; the *resident* arm is gated.
3. **A debug build is not exempted by this and does not need to be.** The bullet above is right that a debug build is a different program rather than a slower one, which is why it reaches every axis of a baseline comparison. Against a budget it is strictly conservative for the same one-sided reason, so `frozen_open` does not refuse one; it prints the profile it was built in and says that a debug FAIL is not attributable. `just gate2` builds `--release`.

**This is not a new `Sensitivity` variant and not a `bench_report` row**, and the distinction is the safeguard rather than a technicality. `Sensitivity` decides what the *report artifact* publishes against a baseline; nothing here changes that, and `tft_open_vs_bag_parse` stays `AbsoluteTiming` and stays `unavailable` — on its missing comparison arm, which no host can fix. The budget is held **outside** the report, in its own binary and recipe, which is `just gate4`'s shape. Gate 4's own justification does not transfer (*"Pss is not a timing measurement"* is exactly what this measurement is not), and this amendment is the justification that does.

What would make this laundering rather than an argument, stated so a later reader can check whether it happened: applying it to a **two-sided** comparison, where a host effect in one direction is a finding; or to a budget whose margin is inside the host's own noise, which is the shape §9.2's *unresolved* verdict already refuses. Both are why the margin has to be printed rather than asserted.

---

## 10. Open-source readiness

Phase 5 is where the repository becomes publishable, so this is a deliverable, not an afterthought.

- **Name check before anything else.** Confirm `tf_tree` is available on crates.io and PyPI, and decide deliberately whether the proximity to ROS's `tf` / `tf2` package names helps discovery or invites confusion. Renaming after 1.0 is not an option; renaming now is an afternoon.
- Apache-2.0 / MIT dual (D30), ~~license headers~~, `NOTICE`, SBOM per release. **The header clause is declined**, with the argument in [`0051`](./decisions/0051-the-licence-travels-with-the-artifact-not-the-file.md): the licence travels with the artifact and is asserted at all three distribution surfaces; a per-file marker is recommended by the Apache appendix and required by neither licence. The other three are done — see §0.0's §10 row for what the SBOM's *done* does and does not cover.
- `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md` with a real disclosure address.
- **A stated support policy**, honestly scoped. A small-team infrastructure project dies from unanswered issues more often than from bad design. Say what is supported, what is best-effort, and what the response expectation is. Under-promising is fine; silence is not.
- **MSRV policy** and a CI matrix pinning it.
- Documentation site (mdBook): a first-five-minutes path that works — `pip install transform_tree`, three lines, a real result — before any architecture prose. **Both halves are open and they are one question**: [`0052`](./decisions/0052-the-first-five-minutes-nobody-runs.md) (`draft`). The three lines exist and run — `README.md`'s *Start with no data at all*, executed out of the README by `scripts/quickstart_smoke.py` on every pull request — but the path they run under is the from-source `just quickstart`, not `pip install`, and nothing anywhere installs the published distribution and imports it.
- CI: the full Phase 1–5 suites on `x86_64` and `aarch64`, ASan/UBSan/TSan, Miri, loom, the nightly `shm_torture`, the benchmark artifact as a regression gate.
- Release automation: `cargo-dist` or equivalent, maturin wheels per Phase 3 §10, PEP 740 attestations, signed tags.

---

## 11. Test plan

- **Three-way bit-identity:** replay one recording into `HeapArena`, `MappedArena`, and `FrozenArena`; identical query set; assert bit-identical `f64`. This extends Phase 2 §10 and is the core correctness claim of the frozen format.
- **Ingest anomalies:** a synthetic corpus containing every row of §3.2, asserting the exact ingest-report output. **Implemented 2026-09-05**, `crates/tf_tree_ingest/tests/anomaly_corpus.rs`. *Exact* is the whole of it: `assert_eq!` on one counter is satisfied by the wrong anomaly being counted, so the assertion is the **entire JSON document, byte for byte**, with only the temporary source path and the crate version interpolated — a field that moves for the wrong reason moves another field with it. **The reportable rows are in that document; two of §3.2's rows are hard errors and produce no report at all** — an edge whose kind changes, and a backward jump past the reset threshold under the default `halt` — so those are driven from the *same* corpus with one message appended rather than from a separate fixture, which is what keeps "every row" a property of one recording. The backward-jump row's other half, a regression below the threshold that §3.2's amendment says is counted and kept because §3.1 sorts, is in the corpus. Red-tested per row rather than per corpus: one seeded violation for each, each failing. **What it does not prove** is stated in the file's own header — the corpus is written by `tf_tree_ingest::fixture`, the writer on the input side of the reader it gates, so it holds this reader's bookkeeping and never its agreement with a real `rosbag2` or DDS writer.
- **Out-of-order ingest:** shuffle a recording's messages; the resulting `.tft` must be byte-identical to one built from the ordered source.
- **Spill path:** ingest with `--max-memory` set below the dataset size; result identical to the in-memory path. **Four tests, because §3.1's amendment splits this into three mechanisms and the third has two properties:** grouping (`capped_memory_matches_the_uncapped_path`), one edge spilled to a run file and merged in a single pass (`an_oversized_edge_spills_and_matches_the_in_memory_path`), a cap small enough that the runs must be *reduced* in several passes first (`a_tiny_cap_reduces_in_several_passes`), and a duplicate `(edge, stamp)` that a reduce pass **re-merges** (`a_reduce_pass_keeps_the_last_occurrence`). The third is not redundant: a single-pass merge over that many runs exceeds the cap tenfold, and only that test observes it. Nor is the fourth: the first two tests are the only ones with duplicates and neither reaches the reduce loop, while the third is the only one that reduces and its fixture has no duplicates — so §3.2's "last wins" across a reduce pass was, until it landed, held by a comment. Every one of these asserts the reported peak from **both** sides; `peak <= cap` alone is satisfied by a path that reports nothing.
- **Chunk decompression:** the newest class §3.3 grew, and it has four parts that are deliberately not interchangeable. **Conformance against real libzstd** (`a_real_libzstd_recording_ingests`, against the committed `testdata/zstd_conformance.mcap`, which the test fails *loudly* on if absent rather than skipping) is a different claim from **round-trip** (`a_zstd_recording_ingests_identically`, `an_lz4_recording_ingests_identically`): an encoder and a decoder from the same crate can agree with each other and both disagree with the zstd `rosbag2` and Foxglove link. **Every bomb guard asserts the allocation and not only the error** — `a_lying_uncompressed_size_is_refused_before_it_allocates` and `a_high_expansion_ratio_is_refused` check `scratch.capacity() == 0` first, because an error alone is also what a reader returns *after* allocating a gigabyte and failing to decode into it; the third guard, on the zstd decoder's declared window, is asserted by `a_zstd_frame_demanding_an_oversized_window_is_refused` and bounded from below by `the_window_floor_admits_what_a_real_zstd_encoder_declares`, because a window bound set too tight refuses ordinary recordings. **Both length disagreements, each written with and without a CRC** (`each_codec_round_trips_and_catches_both_length_disagreements`): `uncompressed_crc == 0` means "not computed" per the specification and real writers emit it, so only the CRC-free rows leave the length comparison as the sole witness. And **the codec-free configuration**, which `--workspace` compiles nowhere and which `just ingest-check` therefore gates on its own, together with a dependency-graph assertion that the shipped CLI's default build still links both codecs. **The asymmetry, stated here because a future implementer reads this list first: the two codecs' conformance evidence differs in kind.** zstd's is a whole recording written by real libzstd. lz4's — there being no `lz4` CLI on the build host — is `a_hand_authored_lz4_frame_decodes_per_the_specification`: 82 bytes written from the LZ4 frame and block formats, asserted not to be what `lz4_flex`'s own encoder emits, and shown load-bearing by `a_single_flipped_bit_in_the_lz4_vector_is_caught` (651 of 656 single-bit perturbations caught, the five don't-cares enumerated against the format text). That is a conformance claim rather than a round-trip one, but it covers one frame and not a file; `testdata/ATTRIBUTION.md` records what would close the rest.
- **Ingest throughput:** §12 criterion 5 had no bullet here either. Time `tf_tree_ingest::run` — *both* passes — against the recording's own stamp span, on a generated corpus at the criterion's own density, in **two** `--max-memory` regimes: one fill pass, and the group count the criterion's own four-hour recording forces. Gate the grouped one. Every premise is checked rather than assumed: each arm asserts the pass count it declares, the density is measured off the survey and floored (a sparser corpus reads arbitrarily higher and would pass without checking anything), and a gated run that would PASS against a `--floor` below the criterion's own refuses — the floor is the whole of the gated comparison, so a caller who may move it downwards can green any run. `crates/tf_tree_bench/tests/ingest_throughput.rs` from `just test` (the binary needs no `shm`); `just gate5` is the gate. [`0050`](./decisions/0050-what-ten-times-real-time-divides.md) is where the four parameter choices are argued.
- **Multi-process page sharing:** 16 processes mapping one `.tft`; assert total RSS is within 1.2× of a single process, measured from `/proc/*/smaps_rollup` `Pss`.
- **Open time:** §12 criterion 2 had no bullet here at all, and now has one. Two arms, and only one of them is a test of this code: with the page cache **resident**, the open of a gate-scale index must fit the 10 ms budget *and* must agree with the open of a fixture two orders of magnitude smaller — that pair is what "an `mmap` plus header validation" asserts. With it **evicted** the number is reported and not gated, because the size dependence there is the storage device. Each open is a **fresh process** (the second open of a file in one process costs a fraction of the first, so a p50 over in-process repeats reports the number that costs nothing) and the verdict takes the worst. Every premise is checked rather than assumed: the evicted arm refuses unless the child's own major-fault count witnesses the eviction, a gated run refuses a fixture under the criterion's own 233 MB scale and refuses two fixtures too close in size for the scale arm to be comparing anything, and a gated run that would PASS against a `--budget-ms` above the criterion's own refuses too — the binary's own header is the enumeration. `crates/tf_tree_bench/tests/gate2.rs` from `just shm-check`; `just gate2` is the gate.
- **Fork safety:** a `DataLoader` with `num_workers=16` under all three start methods.
- **Counter contention:** 16 concurrent readers on one edge, each holding a long-lived `Guard`; assert no measurable throughput difference against a `counters`-disabled build.
- **No network:** full suite under `strace`/seccomp, asserting `socket(2)` is only ever `AF_UNIX` (§5.1). **Scoped to the library's suite** — `tf_tree top --web` is an `AF_INET` listener by construction, and §7's web-view amendment records why that exception belongs in this sentence rather than inside the assertion. **Implemented 2026-09-04**: `just no-network` (`scripts/no-network.sh`), run by `.github/workflows/ci.yml`'s `shm` job on both matrix rows. Every `socket(2)` call the library's test binaries made was `AF_UNIX`; the run prints how many binaries and calls that was, and the figure is not copied here because it moves between runs on one host. The exception is the recipe's **positive control** — a separate trace over `crates/tf_tree_cli/tests/web.rs` that must find `AF_INET` — so "scoped to the library" and "scoped so narrowly it sees nothing" are distinguishable outcomes rather than the same green. It refuses on a missing `strace`, on a `strace` that cannot see a socket it is shown on purpose, on a traced binary that exits non-zero, and on a run in which `tests/rendezvous.rs` was not traced or opened no socket. §5.1's amendment carries the rest, including the one word of §5.1 this corrects.
- **Convenience-path guard reuse:** assert `tree.lookup` in a loop performs O(1) atomic flushes, not O(n).
- **Diagnostics:** one test per check ID, each with a fixture that triggers exactly that check and no other. **Not met, and the two ids that cannot meet it are named in §12 criterion 6 rather than here.** `TFT005` reached this bar on 2026-09-05 — it had no test of any kind and it does not run on the reference fixture either, so neither route reached its firing branch, its tolerance or its message; it now has a unit test that pins `FUTURE_TOLERANCE_NS` at both edges plus an end-to-end one on a wall-clock arena, because the fixture stamps from zero and `Clock::decide` sends it to the `NewestStamp` arm. The remaining gaps are ids reached only through their underlying `doctor::` detector or only through the whole-fixture run, which is not "a fixture that triggers exactly that check"; `rg -n 'fn tft0' crates/tf_tree_cli/src/checks.rs` against the test module is the instrument, and no count of them is written here. **`TFT016` was in neither of those two classes and this sentence did not have a third**, which is how it went for the project's whole life with no test of any kind: its `hostfacts` parsers are not a `doctor::` detector and hold only which files `probe` reads, and the whole-fixture harness passes `host: None` deliberately, so the id was reached by nothing. It reached this bar on 2026-09-06 — a table over synthetic `HostFacts` values that reads no `/sys` and no `/proc`, red-tested against inverting the shmem predicate, against dropping `MCL_ONFAULT` and the *not a clearance* clause from the memlock message (`docs/decisions/0049` corrected both, and the advice that preceded them had shipped), and against the `limit < arena` boundary.
- **`doctor --json`:** schema-validated; adding a check must not break an existing consumer. **Done since 2026-09-05**, and it was unmet for the life of the section: the schema was a rustdoc code block on `render_json` and nothing compared it to the bytes. `catalogue::the_json_report_parses_and_matches_its_documented_schema` runs the real binary, parses stdout as JSON, and holds the document to that block — **which this sentence claimed on 2026-09-05 and was not true until 2026-09-06**: the block is a third spelling beside the `writeln!` emitter and the test's own `expected_keys` literal, the test read only the last two, and emitting a key while adding it to the literal left every check green (the block is fenced ```` ```text ````, so `cargo test --doc` never runs it either). `documented_top_level_keys` parses the block out of `catalogue.rs` now and the literal is held to it before the bytes are held to the literal, so the three move together — **at the top level only**, since the block spells nested shapes inline and `arena`, `arena.rings`, the per-check object, `summary` and the per-finding object are still literals compared to nothing else. What is asserted: the top-level key set in **both** directions, the `tf_tree.doctor/1` identifier against a literal, every catalogue id once and in id order, `reason` a string exactly when `status` is `"skipped"`, and the two different denominators in `summary` (checks for `passed`/`fired`/`not_run`, findings for `error`/`warn`/`info`) against the arrays they summarise — **all three severities**, `uncatalogued` findings included, after a review found the first version compared `warn` alone while this sentence claimed the set; the reference fixture emits no error-severity finding, so that arm catches an emitter that invents a count and nothing about routing, which the test's own doc says. Each rule has a seeded violation recorded in the test's own doc. **No schema *file* is added**: an artifact nothing validates against is decoration, and the block `render_json` already carries is the one a reader lands on. The reader is `serde_json`, a dev-dependency that adds **no package** to the lockfile and nothing to this crate's build graph: `tf_tree_bench` is already a normal dependency of `tf_tree_cli` and declares `serde_json` as a normal dependency of its own. *"Adds nothing to the lockfile"* is what this sentence said and it was wrong — `git diff main...HEAD -- Cargo.lock` adds one `serde_json` edge under `tf_tree_cli`'s entry, which is a dependency edge and not a package. The shipped binary still writes the document by hand, for the reason `render_json`'s doc gives.
- **Web view:** loopback binding asserted; no outbound network requests (assert on the served HTML). **Both are implemented, and two more were needed**: the `Host` guard that makes the loopback bind mean something against DNS rebinding, and an end-to-end test that parses the document a browser receives — every unit test stubs the sampler, so the hand-formatted JSON was otherwise never once parsed.
- **`iter_edge` returns stored samples:** push a known irregular sequence, iterate, and assert the exact stamps come back — no resampling, no interpolation, no reordering.

---

## 12. Gate

1. **Three-way bit-identity passes.**
2. `.tft` open time under **10 ms** for a 233 MB index (it is an `mmap` plus header validation; anything more means work is happening that should not). **MET, and gated since 2026-09-05 — `just gate2`.**

   The parenthesis is the criterion, and it is a statement about *complexity*
   rather than about a duration: every step of `Tree::open_frozen` is O(1) in
   the index size (§2.4's listing, corrected in the same change to name the two
   steps it omitted). So the gate has two halves, and the measured answer is the
   opposite of the one the criterion hypothesises — nothing here does
   size-proportional work, so this is a **regression guard rather than a
   discovery**.

   | | measured on the development host, `--release`, worst of 8 fresh processes — **`just gate2` prints the run's own numbers** | |
   |---|---|---|
   | **budget**, resident page cache | more than two orders of magnitude under the 10 ms budget at 338 MiB | **GATED** |
   | **scale invariance**, resident | a small multiple of the 2.1 MiB fixture's open, well inside the 4× bound | **GATED** |
   | evicted page cache | roughly two orders of magnitude slower than resident at 338 MiB, and much less so at 2.1 MiB | reported |

   **No interval is published in that table, deliberately.** A range over a
   handful of runs is a sample, not the instrument's spread — re-running the
   recipe walks outside a published low end without anything having changed —
   so what is written is the **margin the criterion turns on**, which is what
   survives a host. `just gate2` is the instrument.

   **Only the resident arm gates, and the reason is in the fault counts.** An
   open takes exactly one major fault when the file's cache has been dropped and
   zero when it has not, *at both sizes* — and the whole difference is inside the **one major fault**, not in the steps around it: under `strace -T`, `mmap` reads 21 us against 19 us and `madvise(MADV_HUGEPAGE)` 18 us against 16 us across a 2550x range of mapped length, and the 128-byte header `pread64` 193 us against 167 us. How much the kernel reads to satisfy that one fault is a property of the file and the mapping, not of a step `open_frozen` takes. So the
   evicted arm measures the storage stack under the runner; the arm that is a
   claim about `open_frozen` is the resident one, where the ratio between a
   338 MiB index and a 2.1 MiB one is what "anything more means work is
   happening that should not" actually asserts.

   The words are `evicted`/`resident` rather than `cold`/`warm` because
   `crates/tf_tree_bench/src/bin/attach_bench.rs`, registered in the same
   evidence table, already means something else by *cold* (the first attach in a
   process, explicitly **not** a cold page cache).

   **This publishes an absolute duration on a host that fails `Fitness::probe`,
   and §9.3's one-sided-budget amendment is what admits it** — every check that
   probe fails can only make an open *slower*, so a PASS with margin is a
   conservative claim and a FAIL is not attributable to the code. The verdict
   line prints the fitness reasons whichever way it goes. It is not a
   `bench_report` row and not a new `Sensitivity`: read the amendment before
   copying the pattern.

   **The falsifier is `--prefault`, which edits no threshold**: it reads every
   byte of the index inside the timed region, which is a faithful stand-in for
   the live regression this gate exists to catch — a populate arm reaching the
   frozen backing, which `populate_edge_rings` refuses today by matching on the
   backing rather than by discipline. It puts the open **tens of milliseconds
   over** the 10 ms budget and an order of magnitude past the 4× scale bound —
   both gated halves red, neither marginal. A run it cannot evaluate
   **refuses** rather than passing, and the enumeration lives in the binary's
   own header rather than as a count here — a fixture under this criterion's
   own 233 MB scale; two fixtures too close in size for the scale-invariance
   arm to be comparing anything; a **gated** run whose page-cache eviction did
   not take (an unwitnessed "cold" number is off by two orders of magnitude and
   looks exactly like a fast open); and a gated run that would PASS against a
   `--budget-ms` **above** the criterion's own, because a threshold the caller
   may loosen is a verdict about the argument. The eviction premise is a
   refusal only under `--gate`; an ungated run voids the evicted arm and prints
   why, because that arm gates nothing and the ordinary cause is a **filesystem
   whose pages are RAM** — `$TMPDIR` is a tmpfs on many hosts — which is an
   environment rather than a defect in this code. The two size floors are also
   *disclosed* on an ungated run, so a report never prints a `GATED` line over a
   comparison that is structurally green.
   `crates/tf_tree_bench/tests/gate2.rs` drives all of it through the shipped
   binary, one half at a time, from `just shm-check`.

   **What it does not cover.** §2.2 is explicit that a `.tft` is deliberately
   not prefaulted, so a fast open is by design and the cost is deferred to first
   touch. A gate on the open alone cannot see work *moved* into the first
   lookup. And `tft_open_vs_bag_parse` stays `unavailable` in the report: it is
   a *comparison*, and no artifact holds both of its halves over one recording.
   Criterion 5's `just gate5` times an ingest, but of a fabricated MCAP nothing
   freezes, while this criterion's index was never a bag.
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
   ratio would attribute it to a deeper search that does not exist. **This row is
   not held by nobody**: it is answered, and there is nothing left to hold.
   *That sentence read* **"Unlike criterion 5, this row is not held by
   nobody"** *until 2026-09-05*, and criterion 5 stopped being the foil the day
   `just gate5` started holding it.
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
   > It also found a second, smaller instance of the same class: `just
   > shm-check`'s `--lib` line reaches the library target only, and the
   > `[[bin]]` targets gated on `required-features = ["shm"]` — `owner_migration`
   > among them — are skipped whole by `cargo nextest run --workspace`, so
   > `owner_migration`'s
   > `gate_arithmetic_is_not_vacuous`, which `EVIDENCE.md` cites as the reason
   > that gate's verdict is known to be able to flip, was reachable only by a
   > human typing `just shm-test`. **`just shm-check` now runs `--bins`, and
   > `shm-check` is the recipe `ci.yml`'s `shm` job invokes.**
   >
   > **This paragraph said "had never been executed by any recipe" when it was
   > first written, and that was false** — recorded here rather than rewritten,
   > because the correction is the interesting half. `just shm-test` has run
   > `cargo nextest run -p tf_tree_bench --features shm --bin owner_migration`
   > since before this change, so those five tests did execute; what no
   > workflow did was invoke that recipe (`grep -rn 'just shm-test' .github/`
   > returns nothing, while `just shm-check` is a step of that file's `shm` job —
   > located with `grep -n 'just shm-check' .github/workflows/ci.yml` rather
   > than by a line number, which the next edit above it invalidates). The
   > claim that survives measurement is about CI reach, not about execution,
   > and it is weaker than the one first published. `frozen_workers`'s two
   > arithmetic tests are the ones that had no recipe of either kind — they are
   > new with `--gate`.

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

5. Ingest throughput ≥ **10× real time** on a representative recording. **MET, and gated since 2026-09-05 — `just gate5`.** [`0050`](./decisions/0050-what-ten-times-real-time-divides.md) is the record; it answers what the ratio divides, what the density floor is for, why this may be gated on a host that fails the timing probe, and at what pass count the criterion is stated. Read it before changing any of the four.

   | | measured on the development host, `--release`, worst of 3 rounds, on a 50 edges × 100 Hz × 32 s zstd corpus — **`just gate5` prints the run's own numbers** | |
   |---|---|---|
   | **grouped** — pass two takes the group count this criterion's own recording forces | more than an order of magnitude above the 10× floor | **GATED** |
   | **in-memory** — the default `--max-memory`, one fill pass | also well above the floor, and not what the criterion is stated over | reported |

   **No interval is published in that table, for criterion 2's reason**: a range
   over a handful of runs is a sample rather than the instrument's spread, and
   the margin is what the criterion turns on. `just gate5` is the instrument.

   **The gated arm is the grouped one, and that is the question this row never asked.** `FillStats::passes` counts fill passes and `1` is the ordinary case; `plan_groups` splits pass two whenever the buffered samples exceed `--max-memory`; and **this criterion's own representative recording does not fit the default cap** — four hours at 100 Hz × 50 transforms is 72e6 samples at `SAMPLE_BYTES` = 64, i.e. 4 608 000 000 B against a `DEFAULT_MAX_MEMORY_BYTES` of 4 294 967 296. It gets two groups and one extra whole re-read, so a gate measured at one fill pass and this criterion as written are not the same claim. Each arm asserts the pass count it declares and **refuses** if the run did not take it.

   The **denominator** is the recording's own stamp span (`Survey::span_ns`), and the **numerator** is `tf_tree_ingest::run` — both passes. The **density is measured off the survey and floored** at this criterion's own 100 Hz × 50: "10× real time" is a statement about the corpus as much as about the code, so a sparser corpus **refuses** rather than passing. The falsifier is a **denser corpus**, which turns the verdict red without editing a threshold (measured 2.6× real time at 40× the declared density); `crates/tf_tree_bench/tests/ingest_throughput.rs` drives it, unfenced, so it runs per-PR in `just test`.

   **CORRECTION — three clauses of this row were wrong, and they are quoted rather than edited away.** *It read:* "**Currently held by nobody: `crates/tf_tree_bench/benches/` has no ingest benchmark, so `just bench-check` cannot see a regression on this path.** Measured by hand it passes with a wide margin — 0.048 s for 160 000 transforms through a zstd recording composes to roughly 200× real time for a four-hour bag at 100 Hz × 50 transforms — but a hand measurement is not a gate. Adding one needs a corpus that is *not* produced by `ruzstd`'s own encoder, which understates a real recording's decode cost by about 1.3×; `crate::fixture::compress_records` carries that number."

   (a) The `benches/` premise is true and its implied remedy is not: `just bench-check` runs the `bench_report` binary and gates its `REQUIRED_ROWS`, and executes nothing under `benches/` — `cargo xtask bench-gate` links that suite with `--no-run`. A bench added there would have been run by nothing. (b) **0.048 s is a `survey` figure** — `crates/tf_tree_ingest/src/decompress.rs` says `[crate::survey]` in as many words — reused here as an ingest figure, and the row never states the pass count its extrapolation assumes. (c) The 1.3× is a **per-byte decode-rate** claim, correctly scoped as one where it lives; read as a claim about an *ingest* it does not survive, because a libzstd frame decodes slower per byte **and** there are fewer bytes of it, and the net of those two is derived nowhere in this repository. A producible corpus was named as the blocker while the question nobody had asked was the pass count.

   **Not to be confused with `PHASE4.md` §6.3**, which carries a *different* "10× real time" criterion — ROS 2 bag replay, no drops, bounded queue depth. It is unmet, it is about the bridge rather than about ingest wall time, and nothing here touches it. `just gate5` prints that sentence on every run.
6. Every §6 check has a passing fixture test. **Not met, and it cannot be met
   by writing tests.** `TFT002` (static republished with a different value) and
   `TFT003` (edge kind changed) do not detect in any configuration `doctor`
   builds, so neither has a fixture that could pass: their state is
   `tf_tree_bridge::StaticStore`'s and it is process-local. **Saying that
   accurately is worth more than a partial fix**, so the criterion stays unmet
   rather than being re-read as "every check that *can* detect".

   **What each would need is not the same thing, and neither is a test.**
   `TFT002`'s evidence already exists in `doctor`'s own process on the recording
   source — `tf_tree_ingest` counts `Anomalies::static_conflicts`, and
   `doctor_source` prints that report to stderr and then drops it — so the missing
   piece is a route from the ingest report into `checks::Inputs` **plus a
   decision this document does not make**: the count is a bare `u64` over the
   whole recording with no edge attached, and it counts conflicting
   *observations*, so a latched static re-delivered to late joiners inflates it.
   A `TFT002` finding of that shape is an arena-level `error` whose number grows
   with the number of subscribers, which is the class of misleading figure §6's
   `TFT007` and `TFT010` amendments spend their length refusing — and because
   `TFT002` is `error` severity, it also changes the exit status of every
   existing `--from-bag` CI invocation. Per-edge attribution needs a
   `StaticStore` accessor that does not exist, in a crate this section does not
   own.

   `TFT003` is further away still: the condition **is** detected on a recording
   and is turned into `IngestError::EdgeKindChanged`, which aborts the whole
   run, so *"`TFT003` fired"* and *"the recording ingested"* are today mutually
   exclusive. Making it fire means demoting a hard error to a counted anomaly in
   `tf_tree_ingest`, which changes what `tf_tree ingest` and
   `tf_tree freeze --from-bag` produce — from a refusal to a partial index —
   against §3.2's anomaly list and §5.7's disposition. Doing it inside `doctor`
   alone would be a second ingest policy, which is the second spelling
   `CLAUDE.md` forbids. Both are decision records, not commits.
7. Benchmark artifact runs from the published container on a clean machine and reproduces the committed `results.json` within tolerance.
8. §10 checklist complete, including the name decision.

Criterion 4 is the wedge's central claim, and criterion 7 is what makes it believable to anyone outside the team.

---

## 13. Definition of done

- [x] `FORMAT_VERSION = 3` shipped in a single commit, with Phase 6 **header fields** reserved and `doctor --explain-version` — **this box read `Phase 6 regions reserved` until 2026-09-05 and the region half is retracted (§1.2, [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md)); the box's own evidence clause never verified a region, which is how it stayed ticked.** `crates/tf_tree_arena/src/header.rs` carries `pub const FORMAT_VERSION: u32 = 3` with the header at 320 bytes, `crates/tf_tree_arena/src/layout.rs` **asserts** the resulting `layout_hash()` against a literal rather than intending it, and `tf_tree doctor --explain-version` prints both constants and what a mismatch requires. §0.0's §1 row is the authoritative account, including that two of §1's own amendments were wrong and are corrected in place. **The evidence clause here used to close every property this box names with one "Runs in `just test`", and one of them has no test at all.** The constants are pinned: `layout::tests::layout_hash_is_deterministic_and_stable` asserts `layout_hash()` against the literal and appears in `cargo nextest list --workspace`. The flag is not: `explain_format_version` (`crates/tf_tree_cli/src/lib.rs`) is reached only from the `doctor` arm, `grep -rn 'explain.version' crates/ python/` finds the definition, the clap field and prose about what it prints, and **no test in the workspace list invokes it**. What is pinned is the constants; what the flag prints is unexercised
- [ ] Publish-side observability derived, not counted — push path unchanged and benchmarked to prove it — **the first conjunct is met and the second is not, so the box is not.** **It was ticked on 2026-09-05 and unticked the same day** — ticked on the first conjunct, with the falsifier for the second already written into the same sentence — which is the shape box 6 and box 9 below are left unticked for. Derived is structural: `EdgeCounters` and `ParticipantCounters` (`crates/tf_tree_core/src/counters.rs`) are consumer-side by construction — a denominator and a set of failure causes, every one of them written by whoever *read* — so there is no push counter to switch off. §1.3's `EdgeTelemetry` sketch is the design being rejected, not a type. **"Unchanged" is false as written, and the benchmark the box asks for is what falsified it**: [`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md) put a clock-offset sampler on `EdgeWriter::push`, and `just push-sampler-cost` prices it as a **paired delta in one process** at +1.0–1.1 ns (~21–23 %) on the §11.1 fixture. That artifact is separate from `benches/push.rs` because an unpaired before/after across two `cargo bench` runs on this host read +47 % and the difference between the two answers was drift. Its reading is registered in [`EVIDENCE.md`](./benchmarks/EVIDENCE.md), which `just evidence-audit` holds; it is not a gate and says so. `just push-sampler-cost` is in no workflow — `grep -o 'just [a-z0-9-]*' .github/workflows/*.yml` does not name it — so the number is re-derived by hand. Closing this box means amending its wording against [`0036`](./decisions/0036-the-receipt-time-the-format-already-reserved.md), not ticking it
- [x] Nothing in the codebase, CLI, or docs is called "telemetry" — `grep -rIl -i telemetry` over this repository, excluding `target/`, `.venv*/` and `.git/`, returned **this document alone** on 2026-09-05, and every hit in it is §5.1 arguing the name away or §1.3's rejected `EdgeTelemetry` sketch. Run the grep; there is deliberately no list here. §5.1's amendment records that this was an *enforcement* item rather than a rename pass from the day it was written — and what is still true is that **nothing keeps the word out**: no gate greps for it, so this box states a measurement and not a mechanism
- [x] `socket(2)` restricted to `AF_UNIX`, asserted in CI — `just no-network`, in `ci.yml`'s `shm` job. Scoped to the library's suite per §11; the CLI's one `AF_INET` listener is the check's positive control rather than an exception inside it. **Read the scope with the tick, because it is not the repository**: the five published crates are traced, `tf_tree_cli` is traced as the control, and **every other package here is traced by nothing** (this line used to enumerate them and the enumeration was incomplete). §5.1's amendment states what it does and does not prove
- [x] Error-path counters always on with no runtime switch; `counters` cargo feature is the only knob — `crates/tf_tree_core/Cargo.toml` and `crates/tf_tree/Cargo.toml` both have `default = ["counters"]` and the facade's is a passthrough; the arena regions stay whichever way the feature goes (D34), so turning it off does not fork the layout hash. There is no environment variable and no runtime flag: `grep -rn 'var("TF_TREE' crates/tf_tree_core/src crates/tf_tree/src` finds none, which is §5.3's NORMATIVE rule holding by absence. §5.2's argument for *why* only the denominator was ever expensive is the thing to read before adding a counter. The counters themselves are exercised in `just test` by `crates/tf_tree/tests/counters.rs` — `#![cfg(feature = "unstable")]`, which `cargo nextest run --workspace` unifies in from the crates that turn it on, as that file's own header explains. **The counters-*off* build is compiled, and not by anything this clause named**: `just lint`'s `cargo clippy -p tf_tree_core --no-default-features --features crash-points` for the engine, and `just stable-tier-check`'s `cargo clippy -p tf_tree --lib --no-default-features` for the facade, in `ci.yml`'s `default features (no unstable)` job. **The clause read "a `--features pure-hash` and a no-default-features clippy pass" until 2026-09-05: `pure-hash` is the BLAKE3 backend and has nothing to do with `counters`**, and `just lint`'s own `--no-default-features` lines are `tf_tree_ingest`'s and `tf_tree_cli`'s — `grep -n 'no-default-features' justfile` is the list, and reading it is how this clause should have been written the first time
- [ ] Convenience path holds a long-lived per-thread `Guard` — **not implemented, and §5.4 states it as NORMATIVE.** `Tree::lookup_tagged` builds a `Guard` **per call** — `let g = self.guard();` inside its plan-cache closure, `crates/tf_tree/src/tree.rs` — so the flush-on-drop batching §5.4 designs is one flush per lookup on this path. What *is* per-thread is the plan cache (`crates/tf_tree/src/cache.rs`'s `thread_local!`), which is a different structure solving a different problem. [`0022`](./decisions/0022-the-per-call-guard-and-the-unwatched-gate.md) carries a 2026-08-28 correction confirming exactly this and leaves the per-call guard in place deliberately, answering the cost with `tft_plan_at_many` rather than with a longer-lived handle — so **the box and a decision record disagree about what should exist** — read `0022`'s `**Status:**` line before deciding which of the two moves; closing this box means amending §5.4 or building the guard, not ticking it. §0.0's §5 row reads **Done** over this, because the accumulation half (`Cell<u32>` flushed on `Drop`) *is* wired and the row does not distinguish the two halves
- [x] `freeze --from-live` captures counters into the manifest — **met, and the box's wording is superseded by §5.6's own amendment: they do not go into the manifest, and should not.** §2.1 makes the frozen file an arena *image*, so freezing copies the whole arena and the counter regions land at their own offsets, read back through the identical `ArenaView::edge_counters` accessor a live arena uses. That is the stronger guarantee — there is no code that copies the counters specifically, so there is no code that can forget to — and it avoids a second source of truth. The manifest keeps what the arena cannot hold: source path, recording digest, ingest options. **This tick is not the shape box 2 above declines.** There the second conjunct is simply false and nothing has amended the box; here §5.6 carries the amendment in its own text — *"'into the manifest' was wrong, and §2.1 is why"* — so what is ticked is the section as it now stands rather than the sentence it used to be. The test is `freezing_carries_the_counter_regions` (`crates/tf_tree/tests/frozen.rs`), which makes both a success counter and an error-path counter non-zero before the freeze and reads them back through the frozen view; its own doc comment carries the mutant that kills it. It is `#[cfg(feature = "unstable")]` inside a target whose `required-features = ["shm"]`, so `just test` builds it not at all: `just shm-check`'s `cargo nextest run -p tf_tree --features shm,unstable --test frozen` is the line, in `ci.yml`'s `shared memory` job
- [x] `.tft` format implemented, 2 MiB-aligned arena, CBOR manifest, `source_digest` — `crates/tf_tree_arena/src/frozen.rs` carries `ARENA_FILE_ALIGN = 2 * 1024 * 1024`, the manifest offset/length pair and `source_digest: [u8; 32]`; `crates/tf_tree/src/cbor.rs` is the manifest's **writer**, a definite-length RFC 8949 subset, and there is deliberately **no decoder** — nothing in the read path parses a manifest, so the arena crate takes it as an opaque `&[u8]`. §0.0's §2 row carries the two amendments (the container header's size, and the one-sided per-edge span). Both modules are `#[cfg(all(feature = "shm", target_os = "linux"))]`, so `just test` compiles neither; `just shm-check` runs them as `cargo nextest run -p tf_tree_arena --features shm` and `cargo nextest run -p tf_tree --features shm --lib` / `--test frozen`, in `ci.yml`'s `shared memory` job
- [ ] Three-way bit-identity test green in CI — **the property is composed from three pairwise tests in three crates, and no test drives one recording into all three backings.** Heap against frozen is `a_frozen_lookup_is_bit_identical_to_the_live_one` (`crates/tf_tree/tests/frozen.rs`), compared with `to_bits()`, but its input is a *built* tree rather than a replayed recording; heap against mapped is `a_replay_into_heap_and_mapped_arenas_is_bit_identical` (`crates/tf_tree_cli/tests/replay_bit_identity.rs`), from a recording, compared as raw bits; recording-to-heap against recording-to-frozen is `a_frozen_bag_answers_like_the_tree_it_came_from` (`crates/tf_tree_ingest/tests/frozen_bag.rs`), which compares `Result<Iso3, LookupError>` by **value equality, not `to_bits`** — so the third leg is not a bit-identity assertion at all. §2.1's argument says the three backings are three ways of holding the same bytes; three pairwise tests written against three fixtures do not say it. All three are `shm`-gated, so `just test` compiles none of them; they run through `just shm-check` in `ci.yml`'s `shared memory` job on both matrix rows — which is the "green in CI" half, and it is met
- [x] Offline Python API is the *same* API; no parallel surface introduced — structural rather than promised: `tf_tree.open_file()` returns the ordinary `Tree`, so `plan`, `at` and `span` are the same methods (`python/tf_tree/_core.pyi`). §0.0's §4 row records the way *in* as well — `tf_tree.ingest_bag(path)` returns an ordinary `Tree` ([`0046`](./decisions/0046-the-consumer-the-crate-boundary-was-drawn-for.md)) — and records the second spelling that was **not** added and why: a `freeze_bag` beside it would have been `digest_file` + `run` + `freeze_to` differing only in whether provenance got filled in, and differing *silently*. Gated by the Python suite rather than by `just test`, and that is **two recipes and one interpreter each**: `just py-test` builds into `.venv` (3.14) and `just py-test-freethreaded` into `.venv-t` (3.14t), and `ci.yml`'s `python bindings (pytest, 3.14 + 3.14t)` job runs them as two steps. **This clause read "`just py-test` on two interpreters" until 2026-09-05**; that recipe has no prerequisites and runs `.venv/bin/python -m pytest tests/python` once
- [x] Ingest report emitted as JSON and human summary; every §3.2 anomaly covered by a fixture — `crates/tf_tree_ingest/src/report.rs` carries `to_json` (schema-tagged) and `summary` for the terminal, written next to each other so neither computes what the other does not; the CLI prints the summary and writes the JSON beside the `.tft`. Each row of §3.2's table above has its own test in `crates/tf_tree_ingest/tests/ingest.rs` — `duplicates_resolve_last_wins`, `zero_stamps_are_dropped_and_counted`, `future_stamps_are_kept_and_reported`, `edge_kind_change_is_a_hard_error`, `clock_reset_halts_but_jitter_does_not`, `static_conflicts_are_reported_and_first_wins`, `an_edge_whose_every_sample_was_dropped_is_declared_and_flagged`. Runs in `just test`. **Two residuals, neither of them this box's words**: §11 asks for *one corpus carrying every row*, and coverage here is one fixture per row; and no test compares a whole report against a committed expected document — `report_json_is_well_formed` checks the schema tag, brace balance and the absence of `NaN`/`Infinity`, and says so in its own docstring — so a field that silently stopped being written, or changed units, would fail nothing
- [x] All 16 diagnostic checks implemented with stable IDs, `--json`, and `--exit-code` — **the number in this box is stale downward and must not be read as a target.** `TFT001`–`TFT019` ship; §6's ids are appended and never renumbered, which is what keeps `--suppress` and `--json` compatible across releases, so the count moves in one direction and a box that names it goes stale by design. Recount with `grep -o 'Tft0[0-9]*' crates/tf_tree_cli/src/catalogue.rs | sort -u`. `--json` (schema `tf_tree.doctor/1`), `--exit-code` and `--suppress` are wired in `crates/tf_tree_cli/src/lib.rs`. §0.0's §6 row is the authoritative account of how many *detect*: `TFT002`/`TFT003` are hard-coded skips because the state they would read is `tf_tree_bridge::StaticStore`'s and is process-local, and they say so rather than passing. Runs in `just test` as `catalogue::tests::identifiers_are_unique_and_round_trip` (the ids are unique and round-trip through their strings), `tf_tree_cli::catalogue every_id_is_reported_and_every_skip_states_a_reason`, `the_json_summary_agrees_with_the_exit_status` and `the_exit_code_gate_has_a_warn_tier_and_an_unchanged_default` — all four are in `cargo nextest list --workspace`
- [x] `tf_tree top` TUI plus embedded web view with no build step, loopback-bound
- [ ] `iter_edge` / `iter_edges` / `frame_path` present on both live and frozen arenas, with `iter_edge` yielding stored samples — **none of the three exists in any language**: `grep -rn 'iter_edge\|iter_edges\|frame_path' crates/ python/` returns **nothing at all** — run it and expect empty output, which is the answer and not a failure. (The clause used to end *"nothing but this document's own mentions"*, which that command cannot say: `docs/` is not in its scope. Widen it to `crates/ python/ docs/` and the hits are this box, [`0026`](./decisions/0026-the-corpus-shape-of-a-frozen-index.md) and that record's row in [`decisions/README.md`](./decisions/README.md) — prose, no signature.) It is blocked on a record rather than on effort: [`0026`](./decisions/0026-the-corpus-shape-of-a-frozen-index.md) marks this its step 3 **(Blocked.)**, because the three signatures differ by branch on the per-episode versus per-corpus question — `0026` decides per-episode, so the branch is chosen and what has not happened is the record moving out of draft — read its `**Status:**` line rather than this sentence. Its steps 6 and 7 landed as [`0027`](./decisions/0027-the-48-byte-frame-name-store.md), so the record is partly implemented while its own header still says it is not ready. Its step 2, `freeze_from_arrays`, is the other half §0.0's §3 row records as absent. Before writing a signature: `API.md` §1 R1's three tiers and §7's new-surface checklist, and `CLAUDE.md`'s rule against a second spelling of an existing path
- [x] No viewer dependency, channel, schema, or plugin anywhere in the repository — **this is §8's finished state, not a gap.** No manifest in this workspace names a viewer or a message-transport crate; `grep -rn 'foxglove\|rerun\|rviz\|plotjuggler' --include=Cargo.toml .` is the instrument and returned nothing on 2026-09-05. The only rendering surface that exists is `tf_tree top`'s own — plain ANSI, and a `--web` view over one embedded HTML file with a `default-src 'none'` CSP the server sends, on `std::net::TcpListener` with **no new dependency**. §8.1 carries the argument; `CLAUDE.md` states the consequence for anyone tempted, which is that a viewer integration needs §8.1 refuted first. Nothing enforces this — like the "telemetry" box above, it is a measurement rather than a mechanism
- [ ] Benchmark artifact reproducible from a published container by someone outside the team — **the artifact exists and the container does not.** `just bench-report` / `bench-report-shm` emit `report/{results.json,index.html}` with the §9.3 provenance header and every row `REQUIRED_ROWS` names, `crates/tf_tree_bench/baseline/results.json` is committed, and `just bench-check` gates against it in `ci.yml`'s `bench-gate` job on a fresh runner — so the *clean machine* half already runs. `ls docker/` holds one directory, `tf2`, which is the ROS 2 differential **build** environment and is published nowhere: `grep -rn 'ghcr.io\|docker push\|docker/build-push' .github/ docker/ justfile` returns nothing. Reproduction today is a clone, a Rust toolchain and `just bench-check`. Any container also has to decide which build it ships, because `Tree::open_frozen` is `shm`-gated and the two `.tft` rows are `UNAVAILABLE` without it — which is what `just bench-report-shm` exists to make falsifiable. **Since 2026-09-05 that decision reaches further than the report**: §12 criteria 2 and 4 are held *outside* `bench_report`, by `just gate2` and `just gate4`, and both need a `--release` `--features shm` build, so a container shipping a non-`shm` build reproduces neither — while `just gate5` (criterion 5) needs no `shm` at all and would run in either
- [x] "Where we are worse" section present in the benchmark report — `crates/tf_tree_bench/src/report.rs` carries §9.3's "where `tf_tree` is worse" topics **verbatim**, and §0.0's §9 row records them reaching the emitted report. The residual is on the other side of the same section and is recorded there rather than here, in §0.0's own words: `Report::validate` does not reach every one of §9.3's honesty bullets, and that row's earlier claim that it made the honesty rules structural was true of one bullet and of nothing else. Runs in `just test` as `report::tests::the_where_we_are_worse_entries_are_required_and_must_state_the_cost` and `report::tests::a_worse_entry_with_no_numbers_must_say_why` — both in `cargo nextest list --workspace` — and the emitted artifact is regenerated by `just bench-check` in `ci.yml`'s `bench-gate` job
- [ ] §10 open-source checklist complete, name decision made and recorded — **the name decision is closed and the checklist is not.** [`0008`](./decisions/0008-the-name-tf-tree.md) records it *with the PyPI refusal*, so the distribution is `transform_tree` while the module stays `tf_tree`. §0.0's §10 row is the authoritative account of the rest, and it separates what is **open** from what is **declined** — a distinction this box used to collapse, which is the one a checklist most needs to keep. §10's `license headers` clause is **declined**, not open: neither this box nor that row mentioned it while it was neither done nor declined, and [`0051`](./decisions/0051-the-licence-travels-with-the-artifact-not-the-file.md) now declines it with an argument. Open: §10's `pip install transform_tree` first-five-minutes path, which no workflow executes — [`0052`](./decisions/0052-the-first-five-minutes-nobody-runs.md) holds that question together with the site's; the **mdBook site** — no `book.toml`, no `SUMMARY.md`, no recipe and no workflow, and the honest reason is that nobody has asked for it — and a **signing key**: `release.yml` inspects the tag object and warns, becoming a refusal when the repository variable `REQUIRE_SIGNED_TAGS` is true, and all three paths (signed annotated, unsigned annotated, lightweight) are exercised, but every tag through `v0.0.5` is unsigned — `git cat-file -p <tag> | grep -c 'BEGIN.*SIGNATURE'` answers 0 on each. `CONTRIBUTING.md`'s *Releasing* section carries the one-time setup
- [x] §12 gate met, or a written explanation of which criterion failed and by how much — **the tick records that the explanation exists, not that the gate is met**; the same reading as `PHASE2.md` §15's equivalent box and `PHASE4.md` §9's. **Four states, and the box's wording admits two**: met, failed by a stated margin, **held by nobody** (nothing measures it, so there is no margin to state), and **superseded** — which is not a failure and must not be recorded as one. The criterion the phase's central claim rests on, 1, is *composed* rather than asserted, and that is the row to read first.

  | criterion | verdict | evidence |
  |---|---|---|
  | 1. three-way bit-identity passes | **partly met — composed from three pairwise tests, one of which is not a bit comparison** | §13's box 9 above. Heap↔frozen and heap↔mapped compare `to_bits`; the recording→heap against recording→frozen leg compares `Result<Iso3, LookupError>` by value |
  | 2. `.tft` open under 10 ms for a 233 MB index | **met, and gated since 2026-09-05** | `just gate2`, on a 338 MiB `.tft` this recipe deletes and re-freezes: the gated resident arm clears the 10 ms budget by more than two orders of magnitude, worst of 8 fresh processes. No interval is quoted here — the recipe prints the run's own numbers, and criterion 2 above says why only the resident arm gates. *This row read* **"held by nobody — never measured"**, *and its own last clause predicted the fix*: `report.rs` kept `tft_open_vs_bag_parse` `UNAVAILABLE` while `just gate4` was already freezing an index at this scale, "and the two artifacts do not know about each other". They do now — through a third, `frozen_open`, rather than by joining those two. The report row stays `UNAVAILABLE` on its comparison: criterion 5's recipe times an ingest, but of a different corpus, so there is still nothing to divide this number by |
  | 3. frozen lookup p50 within 20% of online | **superseded, and answered by construction** | §2.1 is NORMATIVE that the frozen read path is the *identical* `Plan::at` against a `PROT_READ` mapping, so the deeper search this criterion prices does not exist. Nothing measures it and nothing is owed; criterion 3's own paragraph carries the argument |
  | 4. 16 workers sharing one `.tft`, total Pss within 1.2× of one worker | **met at 1.024×, and gated since 2026-09-04** | `just gate4`. Criterion 4's own correction records that the binary printed its verdict and returned `Ok(())` on both branches until then, so `nightly.yml`'s `gate4` job could not go red; `crates/tf_tree_bench/tests/gate4.rs` now drives the shipped binary on a fixture that genuinely fails and asserts the exit status, so the distinction is held by a test rather than by whoever next edits the justfile. **They run in different places**: `just gate4` is `nightly.yml`'s `gate4` job, and the test is `#![cfg(all(feature = "shm", target_os = "linux"))]` and runs from `just shm-check`'s `cargo nextest run -p tf_tree_bench --features shm --test gate4`, in `ci.yml`'s `shared memory` job — so the gate is nightly and the assertion that it *can* go red is per-PR. `just gate4-python` **reports** and exits 0 by decision |
  | 5. ingest throughput ≥ 10× real time | **met, and gated since 2026-09-05** | `just gate5`, on a generated 50-edge × 100 Hz × 32 s zstd corpus: the gated grouped arm clears the 10× floor by more than an order of magnitude. No interval is quoted here — the recipe prints the run's own number. [`0050`](./decisions/0050-what-ten-times-real-time-divides.md). *This row read* **"held by nobody"** *and repeated three clauses of criterion 5's own text, all three of which are corrected there*: `bench-check` runs nothing under `benches/`, the 0.048 s figure is a `survey` number, and the 1.3× is a per-byte decode claim rather than an ingest one |
  | 6. every §6 check has a passing fixture test | **partly met, and some ids cannot meet it in any configuration** | `TFT002`/`TFT003` are unconditional skips because the state they would read is `tf_tree_bridge::StaticStore`'s and is process-local — §0.0's §6 row says so rather than passing them. The rest is two-sided. `crates/tf_tree_cli/tests/catalogue.rs` proves the harder property, that a correct fully populated live tree stays quiet — its own header records that every false positive found so far was found there and nowhere else. The other side, that each check *fires* on a hand-built offending state, is the unit tests in `crates/tf_tree_cli/src/checks.rs`, and **which ids they actually reach is a question with an instrument and no answer written here**: `rg -n 'tft0[0-9][0-9]' crates/tf_tree_cli/src/checks.rs`, read against the `mod tests` boundary — the ids that appear below it are the ones a unit test reaches, and this row states no set. Run it before citing this row. `TFT016` was outside that set until 2026-09-06, and it is worth recording because it fitted neither class §11 names as a remaining gap: `hostfacts`' parsers are not a `doctor::` detector and hold only what files `probe` reads, and the whole-fixture run passes `host: None` on purpose, so no route reached any of its messages. Inverting `ShmemThp::honours_madvise`'s use — *healthy host reads broken, broken host reads healthy* — left the crate green. `checks::tests::every_tft016_arm_fires_and_the_two_corrected_strings_are_pinned` is the test that closed it, and closing one id says nothing about the others — run the instrument |
  | 7. benchmark artifact runs from the published container and reproduces `results.json` | **held by nobody — there is no published container** | box 16 above. The clean-machine half runs in `ci.yml`'s `bench-gate`; note also that most rows are `UNAVAILABLE` on both sides on any host, so what the comparison actually holds today is narrow and §0.0's §10 row names it |
  | 8. §10 checklist complete, including the name decision | **partly met — the mdBook site, a signing key, and the `pip install` first-five-minutes path; the header clause is declined** | box 18 above |

- [ ] `docs/PHASE6.md` written, carrying forward the reserved **header fields** and the Phase 4 surprise log — no such file; `docs/` holds PHASE1–PHASE5 and PHASE7. **This box read `carrying forward the reserved regions` until 2026-09-05**, which is §1.2's retracted clause again. The header half of what it would carry is done and asserted — §0.0's §1 row records Phase 6's four header fields as declared absent, with reserved space asserted rather than intended — but the **region table is not reserved**, so a Phase 6 region is a second `FORMAT_VERSION` break. That break is [`0032`](./decisions/0032-the-region-table-was-not-part-of-the-purchase.md)'s subject and is **scheduled rather than owed**: its queue is [`PROJECT.md`](./PROJECT.md) §5.1, the ledger `0032` part 2 opened, and nothing in that ledger is authorised by being listed in it. Read `0032`'s own `**Status:**` line; this box does not restate it, because a status copied into another file is a second place to maintain. `CLAUDE.md` carries the standing consequence, that arena fields must not be added opportunistically. The surprise-log half waits on `PHASE4.md` §1, which is not satisfiable by code. Whatever it says must reconcile with [`0009`](./decisions/0009-descoping-phase-6.md), the record that **cut** covariance, copy-on-write branches, multi-parent edges and URDF-in-the-engine
