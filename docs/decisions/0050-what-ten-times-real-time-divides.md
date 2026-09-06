# 0050: what "ten times real time" divides

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** landed with this record — `just gate5`,
`crates/tf_tree_bench/src/bin/ingest_throughput.rs`,
`crates/tf_tree_bench/tests/ingest_throughput.rs`

## Context

`docs/PHASE5.md` §12 criterion 5 reads *"ingest throughput ≥ **10× real time** on
a representative recording"*, and §13's verdict table records it as **held by
nobody**. That was accurate: nothing in this repository timed an ingest.
`crates/tf_tree_bench` did not even name `tf_tree_ingest` as a dependency, and
its `benches/` directory carries eight criterion benches, none of which ingests
anything.

**The row's stated blocker and its stated remedy were both wrong**, in three
narrower ways than "wrong about everything", each checkable:

1. *"`crates/tf_tree_bench/benches/` has no ingest benchmark, so `just
   bench-check` cannot see a regression on this path."* The premise is true and
   the implied remedy is not: `just bench-check` runs the `bench_report` binary
   and gates its `REQUIRED_ROWS`; it executes nothing under `benches/` at all,
   and `cargo xtask bench-gate` links that suite with `--no-run`. A bench added
   there would have been executed by nothing — the `abi_cost` shape
   `docs/benchmarks/EVIDENCE.md` was created to prevent. (A criterion bench
   *can* have a recipe — `just push-sampler-cost` is one — but it cannot carry a
   verdict, because `criterion` owns the exit status.)
2. *"0.048 s for 160 000 transforms through a zstd recording composes to roughly
   200× real time."* The 0.048 s is a **`survey`** figure — pass one —
   `crates/tf_tree_ingest/src/decompress.rs` says `[crate::survey]` in as many
   words, and a full `run` is about twice it. The row also never states the pass
   count its extrapolation assumes, which turns out to be the load-bearing
   parameter (Q4 below).
3. *"Adding one needs a corpus that is not produced by `ruzstd`'s own encoder,
   which understates a real recording's decode cost by about 1.3×."* That factor
   is a **per-byte decode-rate** claim, and `fixture::compress_records` scopes it
   correctly as one. What it does not survive is being read as a claim about an
   *ingest*: a libzstd frame decodes slower per byte **and** there are fewer
   bytes of it, so the two effects act in opposite directions at the recording
   level and their net is not derived anywhere in this repository.

**Not this criterion.** `docs/PHASE4.md` §6.3 carries a *different* "10× real
time" row — ROS 2 bag replay with no drops and a bounded queue depth — which is
unmet and is about the bridge rather than about ingest wall time. Nothing in this
record or in `just gate5` touches it, and the gate's own verdict output says so
on every run, because the two are one phrase apart and an agent meeting the
phrase will otherwise tick the wrong box.

## Decision

**A `--gate` binary on `just gate4`'s pattern, not a `bench_report` row and not
a criterion bench.** Four questions the code could not answer without inventing
an answer, answered here first.

### Q1 — what does the ratio divide?

* **Numerator**: wall time of `tf_tree_ingest::run` — survey *and* fill. Not
  `survey` alone, which is what §12's own figure was.
* **Denominator**: `Survey::span_ns()`, the recording's own stamp span. That
  accessor already exists (`crates/tf_tree_ingest/src/ingest.rs`); a second
  spelling of `max(newest) − min(oldest)` in the harness would be one more thing
  to drift, and `CLAUDE.md`'s rule against a second spelling applies to a
  benchmark as much as to an API.
* **Worst of N rounds, not the median.** A gate is a worst-case claim and the
  margin here is large enough that the conservative reading costs nothing.

### Q2 — is "10× real time" a claim about the code or about the corpus?

**About the corpus, unless the density is pinned** — at an identical
per-transform cost a 10 Hz × 5-transform recording reads a hundred times higher
than a 100 Hz × 50 one. So:

* the gate **refuses** a corpus sparser than §12's own representative recording
  (100 Hz × 50 = 5 000 transforms per second of recording), because a sparser
  one passes without checking anything;
* the density is **measured off the survey** (`transforms_read / span`), never
  taken from the arguments — which is what lets the same floor guard a corpus
  this harness did not generate;
* and so is the **grouped arm's `--max-memory` cap**, which is the half that
  decides the gated arm's regime and therefore the whole of Q4's answer. It is
  the sum of the largest `ceil(n/2)` edges as pass one measured them, so the
  arms run in the order in-memory-then-grouped. Derived from
  `--edges`/`--rate-hz`/`--seconds` instead, it described the corpus the harness
  *generates* rather than the one it read: under `--reuse-corpus` a run passed
  at exit 0 while being told `--edges 10 --rate-hz 500` about a corpus of 50
  edges at 100 Hz, because the two multiplied out the same. A density measured
  off the survey beside a cap taken from the arguments is not a corpus-agnostic
  gate, whatever the density half is doing;
* transforms per second of wall clock is **reported** beside the ratio, so a
  later change to the corpus shows up as a change to the corpus rather than as a
  win.

### Q3 — may this be gated on a host that fails `Fitness::probe`?

**Yes, under `docs/PHASE5.md` §9.3's one-sided-budget amendment**, which landed
with §12 gate 2 in the same wave and is written to be general rather than to be
about an `mmap`. A floor on a ratio is one-sided: every check the probe fails on
this host — SMT, a busy machine, an unreadable governor — can only make an ingest
*slower*, so a PASS with margin is a conservative claim and a FAIL is not
attributable to the code. The harness prints the fitness verdict and its reasons
whichever way the verdict goes.

**Not `Sensitivity::Ratio`.** That axis's own documentation says it means two
engines interleaved within one round, and this is one engine against a clock;
using it here would be laundering a refusal rather than answering it.

### Q4 — at what pass count is the criterion stated?

**This is the question the row never asked, and it moves the number by ~1.5×.**

`FillStats::passes` counts *fill* passes — its own doc says `1` is the ordinary
case — and the recording is read `1 + passes` times, because the survey reads it
too. The count is not a constant: `plan_groups` splits pass two into groups
whenever the buffered samples would exceed `--max-memory`, and **§12's own
representative recording does not fit the default cap**:

```
4 h × 100 Hz × 50 transforms = 72e6 samples
72e6 × SAMPLE_BYTES (64)     = 4 608 000 000 B
DEFAULT_MAX_MEMORY_BYTES     = 4 294 967 296 B
```

So the criterion's own recording gets two groups and one extra whole re-read. A
gate measured at `passes == 1` and the criterion as written are **not the same
claim**.

The gate therefore measures **both arms and gates the grouped one**:

| arm | `--max-memory` | verdict |
|---|---|---|
| `in-memory` | the default; one fill pass | reported |
| `grouped` | sized for the group count the criterion's own recording forces | **gated** |

and each arm **asserts the pass count it declares**, refusing if the run did not
take it — an arm in the other regime measured a different amount of work, and
comparing the two would be comparing different runs.

## Rationale

**Why not a `bench_report` row.** `REQUIRED_ROWS` is count-pinned to §9.2's table
by `report::tests::the_required_row_set_is_the_size_of_phase5_section_9_2s_table`,
so a row is a two-file edit — but that is the small objection. The real one is
that a row is compared against a committed baseline, which is a *two-sided*
question: being slower and being faster are both findings, so `AbsoluteTiming`
refuses on this host and the row would come back `unavailable`, gating nothing
while looking like a gate. §9.3's amendment is deliberately narrow and does not
extend to a baseline comparison.

**Why not a criterion bench.** `criterion` owns the exit status and has no
verdict to fail on, and nothing runs `benches/` in the go/no-go path anyway.

**Why not a committed libzstd corpus.** The corpus is generated at run time by
`tf_tree_ingest::fixture`. The argument for committing one rested on the 1.3×
decode-rate figure, which is a per-byte claim that does not transfer to an
ingest (Context, item 3); and a committed corpus is megabytes in every clone
plus a build-host dependency on the `zstd` CLI that `crates/tf_tree_ingest`'s own
`[[example]]` comment says **no gate may depend on**. What is given up is stated
rather than hidden: this is a **round-trip** corpus, not a conformance one, and
`testdata/zstd_conformance.mcap` is what closes the conformance half for the
decoder.

**Why zstd rather than uncompressed.** rosbag2 and Foxglove both write zstd
chunks by default, and the uncompressed path is faster — gating on it would
publish the flattering number.

**Why the binary has no `required-features`.** Ingesting into an in-process
`Tree` needs neither `shm` nor the frozen backend, so the binary and its
red test are built and run by `cargo nextest run --workspace`, i.e. by
`just test`, on **every pull request** — which is strictly more than
`frozen_workers`' `shm`-gated shape reaches. Copying that manifest stanza would
have moved the red test into `just shm-check` for no reason.

## Consequences

**A new dependency edge**: `tf_tree_bench` names `tf_tree_ingest`, with
`features = ["fixture"]`. Routine for a `publish = false` harness that already
names `tf_tree_c` and `tf_tree_ipc`, and it costs the lockfile one line —
`git diff Cargo.lock` is `+ "tf_tree_ingest"` under `tf_tree_bench`'s entry, an
edge and not a package, because everything it pulls in was already there.

**A new passthrough feature**, `tf_tree_bench/ingest-compression`, default-on,
rather than `features = ["compression"]` on that dependency — and the reason is
a test in another crate.
`tf_tree_cli::tests::the_cli_compression_feature_switches_the_reader` asserts
that `cfg!(feature = "compression")` in the CLI equals
`tf_tree_ingest::compression_compiled_in()`, and `tf_tree_cli` names
`tf_tree_bench` as a normal dependency. Turning the codec on unconditionally here
would switch the reader under `cargo nextest run -p tf_tree_cli
--no-default-features` and fail that test — which is the *"some dependency is
re-enabling it through its own defaults"* case it was written for, and which
`tf_tree_bench` has already done once, to `counters`. The workspace declares
`tf_tree_bench` with `default-features = false`, so a default-on feature here
reaches `--workspace` and `-p tf_tree_bench` builds and reaches the CLI's edge
not at all.

**What the gate does not measure, stated so nobody reads it as covered:**

* **Conformance.** The corpus's zstd frames are the decoder's own encoder's.
* **A real recording.** `tf_tree_ingest::fixture` fabricates one, and the only
  real transform data in the tree — `testdata/tfstream/indoor_atelier.tfstream` —
  is not an MCAP and is too small.
* **Cold storage — and this was measured rather than assumed.** The generated
  corpus is 3.4 MB, so the read is a small fraction of the ingest: over four
  single-round runs each, an evicted page cache (`dd oflag=nocache
  conv=notrunc,fdatasync count=0`, the same idiom `just gate2` uses) read
  **91.5–165.9×** and a resident one **108.7–166.4×**. The ranges overlap, so on
  a corpus this size the cache state is not separable from this host's
  run-to-run spread, and the record does not claim a penalty in either
  direction. A gigabyte-scale corpus would be a different measurement.
* **A comparison between the two arms.** They are measured in a fixed order —
  in-memory, then grouped — and nothing interleaves them or discards a warm-up,
  so the first absorbs any first-touch cost. On the generated path the ordering
  holds comfortably; on `--reuse-corpus`, the one path where this process did
  not write the corpus, it has been observed to invert. The binary reports the
  ordering it measured and asserts none, and the gated verdict does not depend
  on it: the grouped arm is gated because it is the regime the criterion's own
  words put the measurement in, not because it is the slower one.
* **The four-hour recording itself.** The gated arm reproduces its *pass count*,
  not its size. That the ratio at a given pass count is size-independent to
  first order is an argument about where the work is, not a measurement.
* **`tft_open_vs_bag_parse`.** This does **not** move that row, and the
  distinction is worth stating because the obvious reading is that it does.
  The row's quantity is a *ratio*, and its `Ground::NoInstrument` is a claim
  about the ratio rather than about either half: `just gate2` times the open,
  this gate times an ingest, and no recording is on both sides — the gated
  `.tft` is frozen from a generated fleet and was never a bag, and this gate's
  corpus is a fabricated MCAP nothing freezes. The row stays `unavailable` and
  keeps its ground.

**A number to watch rather than a threshold to trust.** The margin is more than
an order of magnitude at `--release` and about 1.7× in debug on this host, which
is why the per-PR test asserts the red direction and the refusals and leaves the
green direction to `just gate5`.

## Implementation plan

1. **The corrections owed either way**, as their own commit touching no code
   path: `docs/PHASE5.md` §0.0's §3 row, §12 criterion 5, §13's verdict row;
   `crates/tf_tree_ingest/src/lib.rs`, `src/decompress.rs`, `src/fixture.rs` and
   `Cargo.toml`, each quoting what it said. — verified by `just artifact-versions`
   and by `grep -rn "MiB/s" --include=*.rs --include=*.toml --include=*.md .`,
   whose every remaining hit is a **quotation inside a `CORRECTION`** rather than
   a live claim. (Quoting is this repository's convention and the reason the
   figures still appear at all: a claim silently rewritten stops recording that
   it was ever stronger.)
2. **`crates/tf_tree_bench/src/bin/ingest_throughput.rs`**, no
   `required-features`, both arms, the measured density floor, the pass-count
   assertions, `--gate` deciding the exit status. — verified by `just gate5`
   printing a verdict and by its own unit tests.
3. **`crates/tf_tree_bench/tests/ingest_throughput.rs`**, unfenced, driving the
   shipped binary: a denser corpus turns the gate red without editing a
   threshold, the same run exits 0 without `--gate`, `--floor` is the weaker
   falsifier, and both premises refuse. — verified by `cargo nextest run
   -p tf_tree_bench --test ingest_throughput`, and by three seeded mutants listed
   in that file's header, each observed to fail before being reverted.
4. **`just gate5`, a step on `nightly.yml`'s `.tft`-gates job,
   `docs/benchmarks/EVIDENCE.md`'s Gates table.** — verified by
   `just evidence-audit` (a `just lint` dependency, which fails on an
   unregistered target) and `just artifact-versions` (which resolves every
   `just <recipe>` reference in docs and workflows).

## Open questions

None. Q1–Q4 are answered above; what remains is disclosed under *Consequences*
as limits of the measurement rather than as undecided questions, which is the
distinction §12 criterion 5's own row lost — it called a producible corpus a
blocker while the question nobody had asked was the pass count.
