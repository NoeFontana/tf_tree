# 0026: the corpus shape of a frozen index

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none yet

## Context

**Is a frozen `.tft` one file per episode, or one file per corpus?** `docs/PHASE5.md`
§8.3 (`iter_edge`, `iter_edges`, `frame_path`; §4.2's `span` and `manifest`) needs an
episode selector per corpus and none per episode. §3.3's `freeze_from_arrays` is one
call per episode, or a variadic builder that D4 and D10 make a single heap
allocation of the whole corpus.

## Measurements

1000 synthetic episodes (12 frames, 11 edges, 7 212 samples each) built as 1000
per-episode `.tft` (A) and as one prefix-namespaced `.tft` (B). Absolutes are
host-bound; ratios and format properties transfer.

- **Bytes.** The 2 MiB alignment gap is a sparse hole: 2.014 MiB apparent, 21.4 KiB
  on disk per episode. Raw `tar` 3.49× (needs `--sparse`), `rsync -a` ≈ apparent
  (needs `-S`), `cp` 1.00×, any compressed archive removes the hazard.
- **Page sharing** (gate 4's methodology, W=16/W=1): A and B agree to 0.2-0.7 %
  (spawned Python 1.251 vs 1.248, forked 1.050 vs 1.043). Gate 4's 1.024× is a
  Rust-worker statement.
- **Per open file:** +1 fd, +1 VMA, 64 KiB resident. Soft `RLIMIT_NOFILE` 1024 fails
  at 1021 held with `OSError(24)`. B holds one fd and one mapping for any N.
- **Open and query.** Opening every file once, warm: A 25.0 ms, B 0.035 ms; cold: A
  573 ms. `plan.at_into` B/A = 1.000-1.005.
- **Rebuild.** ~2.7 ms and ~0.6 MB peak RSS per episode. Adding or deleting one
  episode is `rm` for A and a full rebuild for B (D4, D10); B at 10⁵ needs ~60 GB.
- **Name truncation.** `FrameRecord::name` is `[u8; 48]` and `for_name` truncates
  without refusal, while `blake3_64` is over the full name. Interning stays exact;
  every reader-visible surface does not:
  1. The prefix budget is 28 bytes for this topology; a natural corpus key is 56.
  2. Partial truncation is invisible: `frames()` returns names that look healthy and
     do not resolve in `plan()`.
  3. The file cannot name its own episodes: the manifest holds no full frame name.
  4. `frames()` is not round-trippable: two frames sharing a 48-byte prefix emit
     identical strings and a copied name answers as the other frame, silently.
  5. `edges()` reports a self-loop, violating D2.

## Decision

**A frozen `.tft` is one file per episode. The corpus is a directory of `.tft`
files plus an index that is not a `.tft`.**

1. **`freeze_from_arrays(frames, edges, stamps, poses, path, *, layout=…)` freezes
   one episode.** One call, one file; no variadic form, no incremental builder. It
   must clear two rules:
   - *§6's "second spelling of an existing path".* `build` + `push_many` + `freeze`
     takes one ring capacity for every edge; `freeze_from_arrays` sizes per edge, as
     §3.1's ingest does. That capability is what makes it a new path.
   - *R4, layout stated, never inferred.* `poses` carries an explicit layout, or the
     docstring says it inherits `push_many`'s. No silent default.
2. **§8.3's three methods keep their signatures**; `ds` is one episode. §4.2's
   `ds.span(...)` stays episode-scoped.
3. **Frame names inside a `.tft` are the robot's own names.** No prefix, no episode
   id (§4.1, the §12 three-way bit-identity test).
4. **Corpus-level identity lives outside the arena**, in an index whose encoding the
   engine does not define. It must carry the **full** episode key, the `.tft` path,
   the episode's `span`, its frame and edge names, and the `source_digest` §2.3
   writes into each manifest. The engine's contribution is at most `tf_tree ingest`
   writing one row per episode. Whatever writes it must read names from its own
   source, not `frames()`/`edges()`, until the 48-byte store is settled.
5. **The dataloader pattern holds a bounded cache of open files.** §4.3's lazy
   post-fork open stays; a cap is added. A shard is not a bound (N = 10⁵, W = 16 is
   6250 files per worker, past the 1021 ceiling); an **LRU of open handles of
   constant size**, independent of N and W, is. The shard remains the right
   *assignment*.
6. **Packaging is sparse-aware**, documented next to the format: raw `tar` wants
   `--sparse`, `rsync` wants `-S`, `cp` needs no flag, compression removes the hazard.

## Rationale

Per-episode's costs are resource costs: loud, numbered and capped by a constant LRU.
Per-corpus's costs are correctness costs on surfaces a consumer reads (items 2-5
above); a silent wrong transform is the class D11 and D15 exist to eliminate. A
synthetic short key needs a side table mapping back to the real key, so per-corpus
would need the same external index while keeping the O(N) rebuild. Corpora grow and
shrink, and the whole arena is one heap allocation.

## Consequences

- **§8.3 and §3.3 unblock.** §8.3 is unchanged; `freeze_from_arrays` gains an explicit
  pose layout (R4) and owes the per-edge-sizing justification.
- **Gate 4's fixture is not the product's shape**; the gate is unchanged.
- **D22.** A `FORMAT_VERSION` bump regenerates per-episode files in parallel and
  restartably, bounded by one episode's memory.
- **The 48-byte truncation is avoided, not fixed**:
  [`0027`](./0027-the-48-byte-frame-name-store.md)'s.

## What this does not decide

- **Gate 4's scope**: cite 1.024× with worker language and start method attached.
- **The 48-byte store.** [`0027`](./0027-the-48-byte-frame-name-store.md).
- **The index's encoding**, and whether it ships here (question 1).

## Implementation plan

Steps 1, 5, 6 and 7 are not blocked on this record's status; steps 2-4 are blocked
on `ready`.

1. **(Not blocked.)** Document the packaging hazard next to §2.3's layout, with the
   table regenerated against `du -s --block-size=1`. Do not advise
   `cp --sparse=always`.
2. **(Blocked.)** `tf_tree.freeze_from_arrays(frames, edges, stamps, poses, path,
   *, layout=…)`. Verified by a test that freezes, reopens with `open_file` and
   asserts `plan.at` bit-identical to the same arrays pushed into a live tree, **and**
   a test that per-edge ring capacities differ when per-edge sample counts differ. If
   the second cannot be written, this step must not land.
3. **(Blocked.)** `iter_edge` / `iter_edges` / `frame_path` on live and frozen trees,
   episode-scoped, `iter_edge` yielding **stored** samples (§8.3 NORMATIVE). Verified
   by exact-stamps and depth-5 `frame_path` fixtures.
4. **(Blocked.)** `PHASE5.md` §2.5 and §4 gain a NORMATIVE paragraph: one `.tft` is
   one episode; corpus identity is external.
5. **(Do not land before question 2 is answered.)** §4.3 gains the constant-size LRU
   with its costs. Verified by a test iterating a generated corpus **larger than the
   cap** under a lowered `RLIMIT_NOFILE` without raising.
6. **Landed as [`0027`](./0027-the-48-byte-frame-name-store.md).**
7. **Landed** as an amendment inside `PHASE5.md` §12 gate 4, which also resolved
   question 3.

## What would make this ready

`draft` authorises no blocked step. It moves to `ready` when questions 1 and 2 are
decided, 4 has found no requester, and 5 is measured.

## Open questions

1. **Does the corpus index belong in this repository?** Recommended: no, beyond
   `tf_tree ingest` writing one row per episode.
2. **Open per item, or a bounded cache, and how big?** No torch `DataLoader` was in
   the loop; one measurement with a real one, sweeping cache size against epoch time
   and resident bytes, sets the constant. Until then step 5 states costs, no cap.
3. **Where does the gate-4 scope finding land?** *Resolved:* a `PHASE5.md` §12
   amendment (step 7).
4. **Is there a requester for a single-file corpus?** None today. If one appears, this
   record reopens and must first refute the ~60 GB peak-RSS arithmetic and the O(N)
   update cost.
5. **What does a variable-length corpus do to the byte totals?** Every measured
   episode is identical; the absolute byte figures are not corpus figures until
   measured.
