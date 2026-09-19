# 0026: the corpus shape of a frozen index

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none yet

## Context

**Is a frozen `.tft` one file per episode, or one file per corpus?** Two
signatures `docs/PHASE5.md` owes differ by branch:

- **§8.3.** `ds.iter_edge(edge, t0, t1)`, `ds.iter_edges(t0, t1)`,
  `ds.frame_path("lidar")`. Per episode they are as written. Per corpus every one
  needs an episode selector (`frame_path("lidar")` is ambiguous 10⁵ ways). §4.2's
  `ds.span(...)` and `ds.manifest` sit on the same fork.
- **§3.3.** `tf_tree.freeze_from_arrays(...)`. Per episode: one call, one file.
  Per corpus: a variadic or an incremental builder, which by D4 and D10 cannot be
  incremental — the whole corpus is one heap allocation before freeze.

§12 gate 4 (1.024×) was measured on one 338 MiB fleet arena; a robot-learning
corpus is 10³–10⁵ short episodes. So the question was measured before it was
decided.

## Measurements

A 1000-episode corpus was built twice from the same synthetic trajectories:
**shape A**, 1000 per-episode `.tft`; **shape B**, the same episodes
prefix-namespaced into one `.tft`. Mobile manipulator, 12 frames, 11 edges, 10.24 s
episodes, 7 212 samples each. Host: 4-physical-core EPYC-Milan guest, Linux 6.8,
ext4, `RLIMIT_NOFILE` and `vm.max_map_count` raised to 1 048 576 (defaults are
1024 and 65 530). Absolutes are host-bound and per §9.3 not gate rows; ratios and
format properties transfer. `tf_tree.build(capacity=)` is one capacity for every
edge, so both shapes overpay identically (common-mode).

**Bytes.** Per-episode fixed overhead is 2.014 MiB apparent (258.7 % of the payload)
but 21.4 KiB on disk (2.68 %): the 2 MiB alignment gap is a sparse hole (`filefrag`:
one block at offset 0, next at byte 2 097 152). It becomes real bytes only when a
copy materialises the hole. Per 100 episodes against 83 968 000 B on disk: raw `tar`
3.49× (needs `--sparse`), `rsync -a` ≈ apparent (needs `-S`), `cp` 1.00× (no flag),
`tar | gzip -1` 0.51×. Any compressed archive removes the hazard. B's manifest is
44.6 B per episode larger (prefixed names); `read_manifest` is on demand, so
`open_file` never parses it.

**Page sharing (gate 4's methodology, Pss behind a barrier, W=16/W=1).**

| start method | A (per-episode) | B (per-corpus) | verdict |
|---|---|---|---|
| spawned Python | 1.2509 | 1.2480 | FAIL |
| forked Python | 1.0504 | 1.0427 | PASS |

The shapes are the same to 0.2-0.7 %; sharding and pre-opening change nothing.
Gate 4's 1.024× is a statement about a Rust worker. Private per-worker cost `p`
and the minimum `S` for `S ≥ 74p`: Rust 0.37 MiB / 27.4 MiB; forked Python
2.24-2.72 MiB / 166 MiB; spawned Python 13.24-13.74 MiB / 980 MiB. This corpus has
S = 787.8 MiB, hence 1.248× spawned. That is a gate-scope finding, not a
per-episode-vs-per-corpus one (*What this does not decide*).

**Per-episode resource cost.** Each `open_file` costs exactly +1 fd, +1 VMA and
64 KiB resident before any query. 1000 open files, zero queries: 86.9 MiB Pss
against B's 25.0 MiB; extrapolated ~6.1 GiB at 10⁵. Ceilings: soft
`RLIMIT_NOFILE` 1024 fails at 1021 held with `OSError(24)` (observed);
`vm.max_map_count` 65 530 at ~65 400 files (arithmetic, not observed). One fd and
one mapping regardless of N for B.

**Open and query.** Opening every file once per epoch, warm: A 25.0 ms, B 0.035 ms (714×);
cold: A 573 ms, B 1.94 ms. A cold epoch of 0.57 s is noise against training: the
open path is not where per-episode loses. `plan.at_into` B/A = 1.000-1.005 (an
778 MiB arena samples as fast as an 814 KiB one); `tree.plan()` compile 1.07-1.13
hammering one episode, 0.82-0.87 at random.

**Rebuild.** Linear: ~2.7 ms and ~0.6 MB peak RSS per episode (N=1000: 2.72 s,
599 MB). A from-scratch build is a wash (A 3.2 ms/episode, B 2.6). Updating is
not: adding or deleting one episode is 3.2 ms / `rm` for A and a full rebuild for
B (D4, D10). Peak RSS extrapolates to ~60 GB at 10⁵, above the 31 GiB host, so B
at 10⁵ cannot be built with the current builder at all. A's peak is one episode
and its build parallelises.

**Name truncation.** `FrameRecord::name` is `[u8; 48]`; `for_name` does
`let n = src.len().min(48)` with no refusal (`crates/tf_tree_core/src/frame.rs:151`),
while `blake3_64` is over the full name. Interning stays exact: two episodes whose
prefixes first differ at byte 54 keep separate data. What breaks is everything a
reader sees:

1. **The prefix budget is 28 bytes** for this topology (`48 − len("camera_optical_frame")`;
   cliff at total length 49). `"ep%06d/"` (9 B) fits and addresses 10⁶ episodes;
   the natural key `"bridge_data_v2/toykitchen2/put_carrot_on_plate/traj0000/"`
   is 56 B before the frame name.
2. **Total truncation is loud; partial truncation is invisible.** At a 29-byte
   prefix `frames()` returns 24 distinct names that look healthy, 4 of which do not
   resolve in `plan()`. `len(name) == 48` is only a hint: a genuine 48-byte name
   trips it.
3. **The file cannot name its own episodes.** With 56-byte prefixes `frames()`
   returns six identical strings and the manifest holds 0 occurrences of the full
   prefix: the `.tft` contains no full frame names.
4. **`frames()` is not round-trippable.** With `V` (51 B) and `T = V[:48]` both
   declared, `lookup` on the real names is correct (−104.0, −99.0); but `frames()`
   emits two identical strings and the one copied out for `V` answers as `T`,
   silently. That hits any consumer that enumerates frames and queries them (a
   corpus index, `doctor`, a plot script), not a caller holding the real names.
5. **`edges()` reports a graph that is not a tree**: two episodes under a 56-byte
   key give one distinct pair, a **self-loop**, violating D2. The stored topology
   is fine; the published one is not.

Also: truncation can split a UTF-8 codepoint, and `frames()` then returns a
replacement character rather than raising. Items 4 and 5 are close to
disqualifying for per-corpus namespacing; items 1-3 mean it cannot work with a
real corpus key.

**Not verified.** Every 10⁵ figure is a linear extrapolation from N = 100-1000.
`fault_around_bytes` is unreadable here, so 64 KiB is a measured `Rss` with a
consistent explanation. Absolute latencies are host-bound.

## Decision

**A frozen `.tft` is one file per episode. The corpus is a directory of `.tft`
files plus an index that is not a `.tft`.**

1. **`freeze_from_arrays(frames, edges, stamps, poses, path, *, layout=…)` freezes
   one episode.** One call, one file; no variadic form, no incremental builder
   (D4/D10 would hold the whole corpus in memory before `finish()`, ~0.6 MB per
   episode). It must clear two rules:
   - *§6's "second spelling of an existing path".* `build` + `push_many` +
     `freeze` takes one ring capacity for every edge, so it cannot produce a
     right-sized arena (this corpus pays 11 × 1024 slots per episode for edges
     that published 11 samples). `freeze_from_arrays` sizes per edge, as §3.1's
     ingest does. That capability is what makes it a new path.
   - *R4, layout stated, never inferred.* `poses` carries an explicit layout, or
     the docstring says it inherits `push_many`'s. No silent default.
2. **§8.3's three methods keep their signatures**; `ds` is one episode, with no
   episode selector. §4.2's `ds.span(...)` stays episode-scoped.
3. **Frame names inside a `.tft` are the robot's own names.** No prefix, no
   episode id: a frozen episode is topologically identical to the live tree
   (§4.1, the §12 three-way bit-identity test).
4. **Corpus-level identity lives outside the arena**, in an index whose encoding
   the engine does not define. It must carry the **full** episode key, the `.tft`
   path, the episode's `span`, its frame and edge names, and the `source_digest`
   §2.3 writes into each manifest. The engine's contribution is at most
   `tf_tree ingest` writing one row per episode as it freezes. Whatever writes it
   must read names from its own source, not `frames()`/`edges()`, until the
   48-byte store is settled.
5. **The dataloader pattern holds a bounded cache of open files.** §4.3's lazy
   post-fork open stays; what is added is a cap. **A shard is not a bound**: at
   N = 10⁵, W = 16 a worker holds 6250 files, past the 1021-file ceiling and
   ~391 MiB resident. What bounds both is an **LRU of open handles of constant
   size** (a few hundred), independent of N and W; the price is a re-open on a
   miss (25.0 µs warm, 573 µs cold, host-bound). The shard remains the right
   *assignment*.
6. **Packaging is sparse-aware**, documented next to the format with the table
   above: raw `tar` wants `--sparse`, `rsync` wants `-S`, `cp` needs no flag,
   compression removes the hazard.

## Rationale

Per-corpus was reached for because the wedge is page sharing; the Pss table
refutes that (the shapes are indistinguishable), so the decision rests on
everything else, which splits cleanly.

**Per-episode's costs are resource costs**: loud, numbered and capped by a
constant LRU (1 fd, 1 VMA and 64 KiB per open file; `OSError(24)` at 1021; 3.49×
through raw `tar`). **Per-corpus's costs are correctness costs** on the surfaces a
consumer reads: a file that cannot name its episodes, a `frames()` that looks
healthy with unresolvable entries, a self-loop in `edges()`, and a copied name that
answers as a different frame. A silent wrong transform is the class D11 and D15
exist to eliminate.

A synthetic 9-byte key fits the budget but needs a side table mapping back to the
real key, so per-corpus ends up needing the same external index while keeping the
rebuild.

The rebuild closes it on memory, not seconds: the incremental cost is O(N) against
O(1), corpora grow and shrink (retention, opt-out, mislabelled episodes), and the
whole arena is one heap allocation (~60 GB at 10⁵).

### Alternatives considered

- **Per-corpus, natural key:** dead on the 28-byte budget and items 3-5.
- **Per-corpus, synthetic key plus side table:** needs the side table anyway and
  keeps the O(N) rebuild.
- **Per-episode, no index (a glob):** `span`, frame sets and provenance would each
  open every file; the index is nearly free to write during ingest.
- **One `.tft` per K episodes:** not measured or adopted. It inherits the
  truncation problem and buys a fraction of a 0.2-0.7 % benefit.

## Consequences

- **§8.3 and §3.3 unblock.** §8.3 is unchanged; `freeze_from_arrays` gains an
  explicit pose layout (R4) and owes the per-edge-sizing justification. The §13
  row for `iter_edge`/`iter_edges`/`frame_path` becomes implementable.
- **Gate 4's fixture is not the product's shape**; the gate is unchanged.
- **D4 and D10 are unchanged**: a frozen arena is never extended, and that is now
  confined to one episode.
- **D22.** A `FORMAT_VERSION` bump invalidates every `.tft`. Per-episode makes
  regeneration parallel, restartable and bounded by one episode's memory; per
  corpus it is one serial, unrestartable rebuild at a peak RSS this host cannot
  supply.
- **The 48-byte truncation is avoided, not fixed.** `edges()` returning a
  self-loop is reachable with one long frame name and no corpus; that is
  [`0027`](./0027-the-48-byte-frame-name-store.md)'s.
- Nothing here changes a crate, a feature, a recipe or the arena layout.

## What this does not decide

- **Gate 4's scope**: 1.024× is a Rust-worker number; under `spawn`, which §4.3
  and `open_file`'s docstring say is CPython 3.14's Linux default, this corpus
  fails at 1.248×. Cite gate 4 with worker language and start method attached.
- **The 48-byte store.** [`0027`](./0027-the-48-byte-frame-name-store.md).
- **The index's encoding**, and whether it ships in this repository (question 1).
- **The chunked middle.**

## Implementation plan

Steps 1, 5, 6 and 7 are not blocked on this record's status; steps 2-4 are blocked
on `ready`.

1. **(Not blocked.)** Document the packaging hazard next to §2.3's layout, with the
   measured table regenerated against `du -s --block-size=1`. Do not advise
   `cp --sparse=always`; `cp` is not a hazard.
2. **(Blocked.)** `tf_tree.freeze_from_arrays(frames, edges, stamps, poses, path,
   *, layout=…)`. Verified by a test that freezes, reopens with `open_file` and
   asserts `plan.at` bit-identical to the same arrays pushed into a live tree, **and**
   a test that per-edge ring capacities differ when per-edge sample counts differ.
   If the second cannot be written, this step has no justification and must not land.
3. **(Blocked.)** `iter_edge` / `iter_edges` / `frame_path` on live and frozen
   trees, episode-scoped, `iter_edge` yielding **stored** samples (§8.3 NORMATIVE).
   Verified by exact-stamps and depth-5 `frame_path` fixtures. Closes a §13 box.
4. **(Blocked.)** `PHASE5.md` §2.5 and §4 gain a NORMATIVE paragraph: one `.tft` is
   one episode; corpus identity is external.
5. **(Do not land before question 2 is answered.)** §4.3 gains the constant-size
   LRU with its numbers (64 KiB per file, `OSError(24)` at 1021, ~391 MiB and
   6250 fds per worker if the cap were a shard). Verified by a test iterating a
   generated corpus **larger than the cap** under a lowered `RLIMIT_NOFILE` without
   raising; a shard-only test would not exercise what the cap is for.
6. **Landed as [`0027`](./0027-the-48-byte-frame-name-store.md).** The 48-byte
   store record.
7. **Landed** as an amendment inside `PHASE5.md` §12 gate 4, which also resolved
   question 3: the worker language and start method attached to 1.024×.

## What would make this ready

`draft` authorises no blocked step. It moves to `ready` when the open questions
are answered: the first two by decision, the fourth by finding no requester, the
fifth by one measurement. Steps 1, 6 and 7 are independent of the fork; step 5 is
not.

## Open questions

1. **Does the corpus index belong in this repository?** Recommended: no, beyond
   `tf_tree ingest` writing one row per episode. Counter: every user writes the
   same 40 lines and half forget `source_digest`. A product-scoping question.
2. **Open per item, or a bounded cache, and how big?** The constant is not derived
   from anything measured, and no torch `DataLoader` was in the loop (the fork and
   spawn arms are raw `os.fork` and `subprocess`). One measurement with a real
   `DataLoader`, sweeping cache size against epoch time and resident bytes, sets
   it. Until then step 5's docstring states the costs but no recommended cap.
3. **Where does the gate-4 scope finding land?** *Resolved:* a `PHASE5.md` §12
   amendment (step 7); nothing about gate 4 is decided, so the qualification sits
   beside the number.
4. **Is there a requester for a single-file corpus?** None today. If one appears,
   this record reopens, and must first refute the peak-RSS arithmetic (~60 GB at
   10⁵) and the O(N) update-and-delete cost.
5. **What does a variable-length corpus do to the byte totals?** Every episode
   measured is identical; real lengths vary by an order of magnitude, which is
   where per-edge sizing stops being common-mode. Nothing in the decision turns on
   it, but the absolute byte figures are not corpus figures until it is measured.
