# 0026: the corpus shape of a frozen index

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none yet

## Context

**Is a frozen `.tft` one file per episode, or one file per corpus?**

Nothing in `docs/PHASE5.md` answers it, and the question is not cosmetic: two
signatures the phase owes cannot be written until it is settled, and they differ
by branch rather than in detail.

- **§8.3.** `ds.iter_edge(edge, t0, t1)`, `ds.iter_edges(t0, t1)`,
  `ds.frame_path("lidar")`. Per episode, all three are as written: `ds` is one
  episode, `edge` is a plain frame-name pair, `t0`/`t1` are that episode's clock,
  and there is exactly one frame called `lidar`. Per corpus, every one of them
  needs an episode selector — `iter_edges(t0, t1)` over 10⁵ interleaved episode
  timelines is not a query anyone asked for, and `frame_path("lidar")` is
  ambiguous 10⁵ ways. These are NORMATIVE lines in a spec and §13's checklist has
  a box for them.
- **§3.3.** `tf_tree.freeze_from_arrays(...)`, which §3.3 says "should exist from
  day one" and which does not exist today. Per episode it is one call, one file,
  one episode's arrays. Per corpus it is either a variadic over episodes or an
  incremental builder with `finish()` — and by D4 (fixed capacity, no growth) and
  D10 (append-only ids) it *cannot* be incremental in the sense a caller would
  assume: the whole corpus is one heap allocation before freeze. The signature
  differs and so does its memory contract.

§4.2's `ds.span(...)` and `ds.manifest` sit on the same fork, less sharply.

**What forces it now** is that everything measured about the wedge assumes one
large arena. §12 gate 4 — "16 workers sharing one `.tft`: total Pss within 1.2× of
one worker", marked **MET at 1.024×** — was measured on a **338 MiB frozen fleet
arena, 64 robots × 40 s, 1 537 frames, 1 536 edges**. A robot-learning corpus is
not that shape. It is 10³–10⁵ short episodes, and if the real unit is an episode
then gate 4 measured a workload nobody runs. §12's own arithmetic is what makes
this worth checking rather than assuming: `(S + 16p)/(S + p) ≤ 1.2` rearranges to
`S ≥ 74p`, so the criterion is arithmetic about process overhead below roughly
27 MiB of touched pages.

So the question was measured before it was decided.

## Measurements

A 1000-episode corpus was built twice on this host from the same synthetic
trajectories: **shape A**, 1000 per-episode `.tft`; **shape B**, the same 1000
episodes prefix-namespaced into one `.tft`. Topology is a mobile manipulator —
12 frames, 11 edges, max depth 5 — with 10.24 s episodes at 100 Hz on seven edges
and 1 Hz on four, 7 212 samples/episode, 7.212 M total. N = 1000 is the low end of
the real range and it is where the common `RLIMIT_NOFILE` default starts to bite.

`tf_tree.build(capacity=)` is **one** ring capacity for every edge, so both shapes
pay 11 × 1024 slots per episode regardless of rate. The waste is identical in A and
B and cancels in every comparison, but it means the absolute byte totals below
overstate what §3.1's per-edge ingest sizing would write. It is common-mode; no
comparison here is affected.

Every number below is labelled **host-independent** (a format or implementation
property, transferable) or **host-bound** (a property of this 4-physical-core
EPYC-Milan guest, which per §9.3 must not be lifted into a gate row).

Host: AMD EPYC-Milan, 4 physical / 8 logical cores, 31 GiB RAM, Linux
6.8.0-136-generic, ext4 on `/dev/sda1`, THP `always [madvise] never`, Python
3.13.12, `transform_tree` 0.0.2. **`RLIMIT_NOFILE` and `vm.max_map_count` are both
raised to 1 048 576 on this box — those are not the defaults a user has (1024 and
65 530), and where that matters it is said.**

### Bytes — host-independent

```
$ stat -c 'apparent=%s blocks512=%b' A/ep000000.tft B.tft
apparent=2930816 blocks512=1640
apparent=818510784 blocks512=1597256
$ du -sh --apparent-size A B.tft ; du -sh A B.tft
2.8G A     781M B.tft
801M A     780M B.tft
$ filefrag -v A/ep000000.tft
 ext: logical_offset: physical_offset: length: expected: flags:
   0:      0..     0:  16548864..16548864:      1:
   1:    512..   715:  16556032..16556235:    204: 16549376: last,eof
```

| | apparent | on disk |
|---|---|---|
| A — 1000 files | 2 930 816 000 B | 839 680 000 B |
| B — one file | 818 510 784 B | 817 795 072 B |
| **per-episode fixed overhead** | **2 112 305 B = 2.014 MiB = 258.7 % of the 797.3 KiB payload** | **21 885 B = 21.4 KiB = 2.68 %** |

**The 2 MiB alignment gap is a sparse hole**, which inverts the scoping worry it
was raised as. `filefrag` shows one block at offset 0 and then nothing until
logical block 512 (byte 2 097 152); ext4 allocates the gap no extents. Decomposed:
128 B header + ~1 330 B manifest + 2 095 694 B hole + 17 250 B of genuine
per-arena structure inside the arena image.

So the "2 MiB per file" cost is **real address space and real apparent size and
almost no disk**. It becomes real bytes only when a copy materialises the hole,
and **which tools do that was measured rather than listed** — an earlier revision
of this paragraph named `cp` as a hazard and it is not one, and quoted a 3.6×
inflation that its own two numbers do not produce:

```
$ tar -cf plain.tar  -C A $(ls A | head -100)   ; stat -c %s plain.tar
293181440
$ tar --sparse -cf sparse.tar -C A $(...)       ; stat -c %s sparse.tar
83875840
$ tar -cf - -C A $(...) | gzip -1 > plain.tar.gz; stat -c %s plain.tar.gz
42712569
$ cp A/ep0000*.tft cp_default/ ; du -s --block-size=1 cp_default
83972096                       # against 83968000 on disk in A -- hole preserved
$ rsync -a A/ep00000*.tft rs_default/ ; du -sb rs_default ; du -s --block-size=1 rs_default
29308160   29331456            # apparent == on disk -- hole materialised
```

Per 100 episodes, against **83 968 000 B** on disk:

| path | bytes | verdict |
|---|---|---|
| `tar` (default) | 293 181 440 | **3.49× — the hazard, confirmed** |
| `tar --sparse` | 83 875 840 | 1.00× |
| `cp` (default `--sparse=auto`) | 83 972 096 | 1.00× — **not a hazard; needs no flag** |
| `rsync -a` (default) | ≈ apparent | **hazard — needs `-S`** |
| `tar` \| `gzip -1` | 42 712 569 | **0.51× — smaller than the on-disk original** |

Three corrections follow, and the third narrows the hazard a long way.
**The factor is 3.49×, not 3.6×** (2 930 816 000 / 839 680 000 = 3.4904; the 3.58
an earlier revision quoted is A-apparent over *B*-apparent, a different pair).
**`cp` needs no flag** — GNU `cp` defaults to `--sparse=auto` and detects the hole;
`rsync` is the tool that does not, and it wants `-S`. And **any compressed archive
removes the hazard outright**, because a hole is a run of zeros: `gzip -1` lands at
*half* the on-disk size. Since a corpus is shipped compressed or as compressed
object-store shards approximately always, this is a footnote for the one workflow
that writes raw `tar` or raw `rsync` — a packaging note, not an architecture
argument, and a smaller one than it first read as.

**The manifest goes the other way.** A's 1000 manifests total 1 329 940 B; B's
single manifest is 1 374 541 B — B pays **44.6 B per episode more**, because every
frame name carries the prefix. It costs nothing at open: `frozen.rs`'s
`read_manifest` is on demand (the fd is retained for it, and the doc comment says
why), so the manifest is never parsed by `open_file`.

### The 1.024× claim — ratios transferable, absolutes host-bound

Measured with gate 4's own methodology: workers actually sweep every declared edge,
and Pss is read from `/proc/<pid>/smaps_rollup` **behind a barrier while all 16 are
alive** — the discipline §12's third bullet insists on, for the reason it gives.

| start method | shape A (per-episode) | shape B (per-corpus) | verdict |
|---|---|---|---|
| spawned Python, W=16/W=1 | 1 052 366 / 841 292 = **1.2509** | 1 023 653 / 820 225 = **1.2480** | FAIL |
| forked Python, W=16/W=1 | 869 690 / 827 990 = **1.0504** | 841 121 / 806 645 = **1.0427** | PASS |

Reproducible to three decimals (repeat runs 1.2481 and 1.0431). No-touch controls
fail loudly — B 8.87×, A 3.37× — so no arm is passing vacuously.

**The decisive observation is that the two shapes are the same.** 1.2509 against
1.2480 spawned; 1.0504 against 1.0427 forked. A 0.2–0.7 % difference. Per-episode
files do not break the page-sharing argument. What breaks it is the worker's
language and start method.

Sharding changes nothing either — 16 workers each covering 1/16 of the corpus give
B 999.3 MiB against 999.7 all-touch and A 1024.0 against 1027.7, within 0.4 %.
Summed Pss is total unique resident bytes and does not care how the work is split.
Pre-opening in the parent and inheriting across fork buys 0.03 %.

**Why 1.024× did not reproduce, and it is not about `.tft`.** Solving
`total(N) = S + N·p` over W = 1 and W = 16 gives the private per-worker cost `p`:

| worker | `p` | minimum `S` for `S ≥ 74p` |
|---|---|---|
| Rust (`frozen_workers.rs`, gate 4's own) | 0.37 MiB | 27.4 MiB |
| forked Python | 2.24 MiB (B) / 2.72 MiB (A) | 166 MiB |
| spawned Python | 13.24 MiB (B) / 13.74 MiB (A) | 980 MiB |

An idle Python worker that opens no file at all costs 24.6 MiB, and 221.2 MiB for
16 — 9.01×. This corpus has S = 787.8 MiB, under the 980 MiB a spawned Python
worker needs, which is exactly why it reads 1.248×. **Gate 4's 1.024× is a
statement about a Rust process**, and the wedge's audience is a Python
`DataLoader` — which §4.3's own amendment and `open_file`'s docstring both say
uses `spawn`/`forkserver` by default on CPython 3.14/Linux.

This is a **gate-scope finding, not a per-episode-vs-per-corpus finding**, and this
record does not decide it. See *What this does not decide*.

### Per-episode resource cost — implementation property host-independent, ceilings host config

```
$ python -c "... open_file('A/ep000000.tft'); count fds, /proc/self/maps, smaps Rss"
delta fds=1 maps=1
arena VMA Rss after bare open_file: 64 kB
```

Each `open_file` costs **exactly +1 fd and +1 VMA**, and **64 KiB resident before
any query is issued** — one Linux fault-around window. (`fault_around_bytes` is not
readable here, so 64 KiB is the measured `Rss` and fault-around is an explanation
consistent with it, not a read of the knob.) A sweep takes the same VMA to 552 KiB
of 814 KiB.

That 64 KiB is where per-episode actually costs resident memory, and it was
measured rather than reasoned: a worker holding 1000 open `.tft` and issuing **zero
queries** holds 86.9 MiB Pss against B's 25.0 MiB. The 62 MiB gap is 1000 × 64 KiB.
Extrapolated, a worker holding 10⁵ episode files pays **~6.1 GiB resident before
doing any work**.

Held-open ceilings, pushed to failure:

| limit | value | observed |
|---|---|---|
| `RLIMIT_NOFILE` soft = 1024 (distro default) | ~1020 files | **1021 held, then `OSError(24, 'Too many open files')`** — observed directly, loud, not silent |
| `vm.max_map_count` = 65 530 (Ubuntu default) | ~65 400 files | **not observed** — lowering it needs root. Arithmetic from the measured +1 VMA per open |
| this host, both raised to 1 048 576 | — | 200 000 simultaneous maps, no error, no tf_tree-internal limit in the way |

So the binding-constraint order is `RLIMIT_NOFILE` first, `vm.max_map_count`
second. Shape B needs one fd and one mapping regardless of N.

### Open and query cost — ratios transferable, absolutes host-bound and NOT gates

Warm, one worker opening every file once per epoch: A **25.0 ms** (25.0 µs/open
including close) against B **0.035 ms** — 714×. Cold (`POSIX_FADV_DONTNEED` first,
eviction verified with `mincore`: 0/716 and 0/199 832 pages resident): A **573.3 ms**
against B **1.94 ms** — 295×. **In absolute terms even A's cold epoch is 0.57 s,
which is noise against a training epoch. The open path is not where per-episode
loses.**

A surprise worth keeping: repeat-opening the **one big file** costs 31.5 µs/open
warm, *more* than opening a small one (15.2 µs). `munmap` of an 816 MiB VMA is more
expensive than of an 814 KiB one, so "open it in the worker" is cheaper per call
for shape A than for shape B.

Query, interleaved within-round with the leading arm alternating, median per-round
quotient, 800 rounds, 3 runs (§9.3's Ratio method):

| row | hammering one episode | visiting episodes at random |
|---|---|---|
| `plan.at_into`, K = 1024 | B/A = 1.000, 1.000, 1.001 | 1.003, 1.005, 1.002 |
| `tree.plan()` compile | 1.108, 1.069, 1.126 | **0.867, 0.852, 0.820** |

**A 778 MiB arena samples exactly as fast as an 814 KiB one.** The binary-search
depth difference does not exist, because the ring is per-edge and both are
log₂ 1024. Plan compilation is the only row that moves, and it moves *toward* B
under random access — B's single frame table stays warm where A pays a cold
frame-table and plan-cache miss per tree. Absolutes (~689 ns/stamp hot,
~744–752 ns/stamp random, depth-6 all-dynamic under ScLerp) are host-bound on this
contended 4-core guest and are **not** gate rows.

### Rebuild — slope host-independent, absolutes host-bound

D4 fixes capacity and D10 forbids id recycling, so episode N+1 cannot be appended
to a frozen corpus index. It is rebuilt.

| N | build+freeze | peak builder RSS |
|---|---|---|
| 100 | 0.28 s | 91 508 kB |
| 250 | 0.65 s | 173 276 kB |
| 500 | 1.29 s | 309 572 kB |
| 1000 | 2.72 s | 599 140 kB |

Linear: ~2.7 ms and ~0.6 MB of peak RSS per episode.

**The seconds are the weaker half of this argument and the counter-number is stated
here rather than left for a reader to find.** Building a corpus *from scratch* is a
wash, and if anything favours B: re-measured at N = 100 while reviewing, shape A
takes **0.32 s (3.2 ms/episode)** and shape B **0.26 s (2.6 ms/episode)**, so at 10⁵
the full builds extrapolate to ~320 s and ~260 s respectively. Per-corpus is the
*faster* of the two to build once.

What is not a wash is **updating**. Adding one episode costs shape A **3.2 ms** (one
new file) and shape B a **full rebuild** — 2.72 s at N = 1000, ~260 s extrapolated at
10⁵ — because D4 fixes capacity and D10 forbids recycling, so there is no append.
The same asymmetry applies to *deletion*, which a real corpus needs for retention,
opt-out and mislabelled episodes: per-episode is `rm`, per-corpus is another full
rebuild.

And the decisive number is memory, not time. Peak builder RSS extrapolates to
**~60 GB, above this host's 31 GiB** — so **shape B at 10⁵ episodes cannot be built
on this machine with the current builder at all**, because the whole arena is one
heap allocation before freeze. Shape A's peak is one episode's worth whatever N is,
and its build is embarrassingly parallel across cores and machines where B's is a
single process. That is the axis on which the two shapes are not close; the
wall-clock rows are not.

### Name truncation — host-independent, and it is the heart of the matter

`FrameRecord::name` is `[u8; 48]` and `for_name` does `let n = src.len().min(48)`
with no refusal (`crates/tf_tree_core/src/frame.rs:151`). `blake3_64` is taken over
the **full** name (`frame.rs:102–108`) and only the *stored copy* is truncated.

**The scoping pass's worry was that per-corpus namespacing would silently merge two
episodes. That is refuted.** Interning is exact: two episodes whose 56-byte
`BridgeData`-shaped prefixes first differ at byte 54 keep entirely separate data
(`traj0000` tx = 2.0, `traj0001` tx = 4.0, exactly as pushed). The engine does not
confuse them.

**What is broken is everything a reader can see**, and one thing worse than that.

```
$ python  # 2-edge tree, prefix of length L on 'base_link' and 'camera_optical_frame'
prefix=24B distinct=2/2 lens=[33, 44] roundtrip=['ok', 'ok']
prefix=28B distinct=2/2 lens=[37, 48] roundtrip=['ok', 'ok']
prefix=29B distinct=2/2 lens=[38, 48] roundtrip=['ok', 'FrameNotDeclaredError']
prefix=33B distinct=2/2 lens=[42, 48] roundtrip=['ok', 'FrameNotDeclaredError']
```

1. **The prefix budget is 28 bytes for this topology**, found by binary search on
   the observable rather than computed from the constant: `48 − len("camera_optical_frame")`.
   The cliff is exactly at total name length 49. A minimal synthetic key fits —
   `"ep%06d/"` is 9 B and addresses 10⁶ episodes. **The natural key does not:**
   `"bridge_data_v2/toykitchen2/put_carrot_on_plate/traj0000/"` is 56 B before the
   frame name.
2. **Total truncation is loud; partial truncation is invisible.** At a 29-byte
   prefix only the longest frame names cross 48. `frames()` then returns 24
   entries, **24 distinct — it looks completely healthy** — and 4 of them do not
   resolve when fed back to `plan()`. The nearest thing to a tell is
   `len(name) == 48`, and it is **not a sound one** — see below; a genuine 48-byte
   name trips it too. The run above shows the two-frame version of the same thing:
   two distinct names, one of which is not a name.
3. **The file cannot name its own episodes.** With 56-byte prefixes, `Tree.frames()`
   returns six *identical* 48-byte strings, 1 distinct of 6, and `Tree.edges()`
   returns 1 distinct pair of 4 — and that pair is a self-loop, which is item 5.
   Grepping the CBOR manifest finds **0** occurrences
   of the full prefix and 6 of the truncated one. **The `.tft` does not contain the
   full frame names anywhere**, so a corpus-wide index cannot tell you which
   episodes are inside it and nothing in the file can recover them.
4. **`frames()` is not round-trippable, and when two names share a 48-byte prefix
   the round trip is silently wrong rather than loudly rejected.** An earlier
   revision of this item claimed something stronger — that a `lookup` on the *full*
   name resolves to the shorter frame and returns its transform. **That is refuted.**
   Re-run independently while reviewing this record:

```
$ python  # V = 51 B; T = V[:48] declared as a SEPARATE frame; root->V tx=-104, root->T tx=-99
frames() = 3 entries, 2 distinct; 48-byte entries: 2, distinct among them: 1
lookup('root', V=51B) tx = -104.0   <- truth for V is -104.0   CORRECT
lookup('root', T=48B) tx =  -99.0   <- truth for T is  -99.0   CORRECT
string copied out of frames() -> lookup tx = -99.0
```

   The engine never confuses `V` and `T`: `blake3_64` is over the full name, and
   `FrameRecord::name_matches` (`frame.rs:171`) compares truncated bytes only as a
   hash-collision tiebreak, which is sound for that job. **The hazard is entirely in
   the output-then-reuse loop**: `frames()` emits two byte-identical strings, and the
   one a user copies out for `V` is `T`'s name and answers as `T`, with no error.
   That is a real silent wrong answer for any consumer that enumerates frames and
   queries them — a corpus index, `doctor`, a plotting script — and it needs an
   unlucky 48-byte name, which a prefix scheme that truncates at 48 makes *more*
   likely because it manufactures names that all end at exactly 48. It is not a
   wrong answer to a caller who has the real names in hand.

5. **`edges()` reports a graph that is not a tree**, which is worse than item 3 and
   was missed until review. With two episodes under a 56-byte key:

```
$ python  # traj0000 and traj0001, map->odom->base_link each
frames(): 6 entries, 1 distinct
edges():  4 entries, 1 distinct -> [('bridge_data_v2/toykitchen2/put_carrot_on_plate/t',
                                     'bridge_data_v2/toykitchen2/put_carrot_on_plate/t')]
SELF-LOOPS in edges(): 1
engine identity still exact: traj0000 tx=2.0, traj0001 tx=4.0
```

   A single **self-loop**, parent to itself. Any consumer that reconstructs topology
   from `edges()` — which is exactly what a corpus index or a `frame_path`
   implementation does — builds a cycle, in a project whose D2 is *a tree, not a
   pose graph*. The stored topology is fine; the published one is not.

Two smaller things, both measured, neither decision-changing. **`len(name) == 48` is
not a sound tell** for item 2's partial truncation: a genuine 48-byte frame name
resolves correctly and trips the same test, so the check has false positives and can
only ever be a hint. And **truncation can split a UTF-8 codepoint** — 47 ASCII bytes
followed by `中` stores an invalid-UTF-8 48-byte name, and `frames()` returns
`'uuu…u�'` from both the live tree and the reopened `.tft`, substituting a
replacement character rather than raising.

So the earlier worry was wrong in its stated form, and two reader-visible failures
are confirmed in its place — one silent, one structural. **Items 4 and 5 together are
close to disqualifying for per-corpus namespacing**, and items 1–3 mean the scheme
cannot be made to work with a real corpus key even if neither ever fired.

## Decision

**A frozen `.tft` is one file per episode. The corpus is a directory of `.tft`
files plus an index that is not a `.tft`.**

Concretely:

1. **`freeze_from_arrays(frames, edges, stamps, poses, path, *, layout=…)` freezes
   one episode.** One call, one file. No variadic-over-episodes form, no incremental
   builder — D4 and D10 mean an incremental builder would hold the whole corpus in
   memory before `finish()` (measured: ~0.6 MB peak RSS per episode), which is a
   contract no caller would guess from the name.

   **It has to clear two rules this repository has already used to kill a §3.3/§4.2
   entry, and an earlier revision of this record cleared neither.**

   - *§6's "second spelling of an existing path".* §4.2's amendment deleted
     `resample` for being `plan.at(np.arange(...))` in a wrapper. `freeze_from_arrays`
     invites the same objection: `tf_tree.build(edges, capacity=) + push_many +
     freeze(path)` is five lines and is literally what this record's own `build_a.py`
     does. **The answer is in the Measurements above and has to be stated in the
     signature's justification, not assumed:** `tf_tree.build(capacity=)` takes *one*
     ring capacity for *every* edge, so the existing path cannot produce a
     right-sized arena — this corpus pays 11 × 1024 slots per episode for edges that
     published 11 samples. §3.1's ingest sizes per edge exactly. `freeze_from_arrays`
     is that capability, which the existing path does not have; that is what makes it
     a new path rather than a second spelling, and it is the only thing that does.
   - *R4 — layout stated, never inferred.* The signature above carries an explicit
     layout for `poses`; the earlier four-positional-argument form did not. R4's own
     words are that `wxyz` versus `xyzw` is "a different, still-unit quaternion" that
     produces "a valid-looking transform pointing the wrong way", and §6 names
     *giving a layout parameter a default* as a smell. Either name the convention in
     the signature or inherit `push_many`'s and say in the docstring that it is
     inherited — silently is not one of the options.
2. **§8.3's three methods keep the signatures §8.3 already gives them.**
   `ds.iter_edge(edge, t0, t1)`, `ds.iter_edges(t0, t1)`, `ds.frame_path(name)` —
   `ds` is one episode, and no episode selector appears in any of them. §4.2's
   `ds.span(...)` likewise stays an episode-scoped answer, which is the only scope
   at which "the interval over which every dynamic edge on the plan has data" means
   anything.
3. **Frame names inside a `.tft` are the robot's own names.** No prefix, no episode
   id, no namespace. A frozen episode is topologically identical to the live tree it
   was frozen from, which is what §4.1's "identical to online" already promises and
   what the three-way bit-identity test in §12 already assumes.
4. **Corpus-level identity lives outside the arena**, in a side index the engine
   does not define a binary format for. What it must carry is fixed here even
   though the encoding is not: the **full** episode key (the 56-byte one), the
   `.tft` path, the episode's `span`, its frame and edge names, and the
   `source_digest` §2.3 already writes into each file's manifest. The engine's
   contribution is at most `tf_tree ingest` writing one row per episode as it
   freezes; the index format itself is the user's.
5. **The documented dataloader pattern holds a bounded cache of open files.**
   §4.3's lazy post-fork open stays exactly as it is; what this adds is a *cap*.
   **A shard is not a bound** — an earlier revision said "open per shard" and that
   was wrong at the top of this record's own stated range. A shard is `N/W`: at
   N = 10⁵ and W = 16 a worker holds **6250** files, which is past the measured
   1021-file ceiling under the default `RLIMIT_NOFILE` and costs
   6250 × 64 KiB ≈ **391 MiB** resident. It pays *both* costs, 16× smaller — not
   neither. What bounds them is an **LRU of open handles whose size is a constant**
   (a few hundred), independent of N and W: fds and VMAs stay flat, resident
   fault-around stays flat, and the price is a re-open on a miss, measured at
   25.0 µs warm and 573.3 µs cold on this host (host-bound, and both are noise
   against a training step). The shard is still the right *assignment* — it keeps
   the miss rate low — but the cap is what makes the numbers bounded.
6. **Packaging is sparse-aware, and the hazard is narrower than it looks.**
   Documented next to the format, with the measured table above: raw `tar`
   inflates 3.49× and wants `--sparse`; `rsync` inflates and wants `-S`; **`cp`
   needs no flag**, its `--sparse=auto` default already preserves the hole; and any
   compressed archive removes the hazard outright (`tar | gzip -1` lands at 0.51×
   of the on-disk size, because a hole is a run of zeros). Since corpora ship
   compressed, this is a note for the raw-`tar`/raw-`rsync` workflow only.

## Rationale

**The obvious argument for per-corpus does not survive measurement, and it was the
only argument that would have been decisive.** Per-corpus was reached for because
the wedge is page sharing and one file looked like the way to share pages. It is
not: 1.2509 against 1.2480 spawned and 1.0504 against 1.0427 forked. The shapes are
indistinguishable on the wedge's central claim, so the decision has to be made on
everything else — and everything else splits cleanly.

**Per-episode's costs are resource costs, with loud failures and a bounded
mitigation.** One fd, one VMA and 64 KiB resident per open file; a hard
`OSError(24)` at 1021 with the default `RLIMIT_NOFILE`; a 3.49× apparent-size
inflation through raw `tar` or raw `rsync`. Every one of those is visible, has a
number, fails noisily, and is capped by a constant-size LRU of open handles — *not*
by sharding, which only divides them by `W`. The open path itself is not a problem
at all: A's *cold* epoch is 0.57 s.

**Per-corpus's costs are correctness costs, and they land on the surfaces a
consumer reads.** A user who prefixes with the natural corpus key gets a file that
cannot name its own episodes (measured: 0 occurrences of the full key in the
manifest), a `frames()` list that can look 24-of-24 healthy while 4 entries are not
names, an `edges()` that reports a **self-loop** where a two-episode tree should be,
and a `frames()` string that when copied back into `lookup` answers about a
different frame with no error. The engine's own identity survives all of it —
`blake3_64` is over the full name — so nothing here is a wrong answer to a caller
holding the real names. What breaks is every consumer built on the published
topology, which is what a corpus index *is*. A silent wrong transform is the failure
class this project's D11 and D15 exist to eliminate, and adopting a layout that
manufactures 48-byte names would be walking toward it deliberately.

**And the escape hatch from the truncation problem removes per-corpus's last
advantage.** A 9-byte synthetic key (`"ep%06d/"`) fits the 28-byte budget and
addresses 10⁶ episodes. But a synthetic key is meaningless without a side table
mapping it back to `bridge_data_v2/toykitchen2/put_carrot_on_plate/traj0000`, and
that table lives outside the `.tft` — so the per-corpus design ends up needing
**the same external index the per-episode design needs**, while keeping the
rebuild. It buys one fd and one VMA and gives up self-description to get them.

**The rebuild is what closes it, and it closes on memory rather than on seconds.**
Because D4 fixes capacity and D10 forbids recycling, a corpus index cannot be
appended to or deleted from; adding or removing one episode is a whole rebuild.
The wall clock is *not* the argument — measured, a from-scratch corpus build is a
wash and slightly favours B (2.6 ms/episode against A's 3.2) — so the honest claim
is narrower than "rebuilding is slow". It is that the incremental cost is **O(N)
where per-episode is O(1)**, that corpora grow *and shrink* by construction
(retention, opt-out, mislabelled episodes), and that peak builder RSS extrapolates
to **~60 GB against this host's 31 GiB**, because the whole arena is one heap
allocation before freeze. A format that cannot be built at the top of its own
stated range on a developer machine, and whose every update and deletion is a full
rebuild, is not the unit of storage.

### Alternatives considered

- **Per-corpus with the natural key.** Dead on the 28-byte budget alone: the key is
  56 B before the frame name. Also dead on items 3, 4 and 5 above.
- **Per-corpus with a synthetic short key plus a side table.** Viable, and rejected
  because it needs the side table anyway (so it has no advantage over the decision)
  while keeping the O(N) rebuild and the 60 GB extrapolation (so it has a
  disadvantage).
- **Per-episode with no index at all** — the corpus is a glob. Rejected: `span`,
  frame sets and provenance would each be answered by opening every file, and the
  index costs almost nothing to write during ingest, which is the only moment the
  full keys are still in hand.
- **A "chunked" middle — one `.tft` per K episodes.** Not measured and not adopted.
  It inherits the truncation problem in full (it still namespaces) and buys a
  fraction of a benefit that measured at 0.2–0.7 %. If someone wants it, the
  argument has to start by refuting the Pss table.

## Consequences

- **§8.3 and §3.3 unblock.** §8.3's three methods keep the signatures they already
  carry, unchanged. §3.3's `freeze_from_arrays` does **not** — it gains an explicit
  pose layout (R4) and owes the per-edge-sizing justification in Decision item 1,
  without which it is a §6 second spelling of `build` + `push_many` + `freeze`. The
  §13 checklist row for `iter_edge`/`iter_edges`/`frame_path` becomes implementable.
- **§12 gate 4's fixture is not the product's shape, and this record does not
  change the gate.** Gate 4 stays a 338 MiB single-arena measurement, which is a
  legitimate thing to measure — it is just not a corpus. What it is *evidence for*
  is narrower than it reads, and that is a separate finding (below).
- **D4 and D10 are unchanged and their consequence becomes explicit.** Fixed
  capacity and append-only ids mean no frozen arena is ever extended. Under this
  decision that consequence is confined to one episode: an episode is immutable
  once frozen, which is what an episode is. It stops being a corpus-scale
  liability.
- **D22 and the re-freeze cost.** A `FORMAT_VERSION` bump invalidates every `.tft`
  in existence — §1 already says every participant must be rebuilt and restarted
  together, and a frozen corpus is a participant that cannot be restarted, only
  regenerated. This decision makes that regeneration **parallel and restartable**:
  10⁵ independent 3.2 ms re-freezes (host-bound absolute; the transferable part is
  that they are independent), resumable after a crash, each bounded by one episode's
  memory. Per corpus it would be one serial rebuild, unrestartable, at a peak RSS
  this host cannot supply. D22's argument is about not forking the layout hash; its
  *operational* half is that a bump has to be survivable, and per-episode is the
  shape that makes it so. **The wall clock is not the point here either** — the
  serial rebuild's ~260 s extrapolated is not by itself a problem; being serial,
  unrestartable and 60 GB is.
- **The 48-byte truncation is not fixed by this decision, only avoided.** A robot's
  own frame names are comfortably under 48 bytes, so the common case stops being
  exposed — but the reproductions above use one tree and no corpus at all, and one
  of them (`edges()` returning a self-loop) is reachable by any user with a long
  frame name, corpus or not. It needs its own record; see below.
- **A consumer that trusts `edges()` can build a cycle.** Not created by this
  decision and not fixed by it, but named here because the decision's own item 4 —
  a side index carrying frame and edge names — is exactly such a consumer. Whatever
  writes that index must read names from its own source, not from `frames()`/
  `edges()`, until the 48-byte record is settled.
- **`just` recipes and CI are untouched.** Nothing here changes a crate, a feature
  or the arena layout.

## What this does not decide

- **Gate 4's scope, and it is arguably the larger finding.** §12 gate 4's 1.024× is
  a Rust-worker number (p = 0.37 MiB). A Python worker's p is 2.24 MiB forked and
  13.24 MiB spawned, so the gate's own `S ≥ 74p` puts the minimum passing corpus at
  166 MiB and 980 MiB respectively — and under `spawn`, which §4.3 and `open_file`'s
  docstring both say is CPython 3.14's default on Linux, the 788 MiB corpus measured
  here **fails at 1.248×**. If gate 4 is cited as evidence for the wedge, it must be
  cited with the worker language and start method attached. That belongs in a §12
  amendment or its own record, not buried here.
- **The 48-byte store.** Whether `for_name` should refuse a name over 48 bytes,
  whether the manifest should carry full names, or both. Items 4 and 5 above are
  both reachable without any corpus — a self-loop in `edges()` needs only one
  over-long frame name — and they are the strongest argument for a loud refusal; the
  counter-argument is that a refusal is a breaking change to `TreeBuilder`. Note that
  a refusal fixes item 5 outright but only *converts* item 4's silent case into a
  build-time error, which is the right trade but is a different claim. Own record.
- **The index's encoding.** JSON lines, parquet, sqlite — this record fixes what the
  index carries and that it is not a `.tft`, not how it is spelled.
- **Whether the index ships in this repository at all.** See open question 1.
- **The chunked middle.** Named as an alternative, not measured, not scheduled.

## Implementation plan

Steps 1, 5, 6 and 7 are **not** blocked on this record's status and can land while
it is `draft`; steps 2, 3 and 4 are blocked on it reaching `ready`, because they
write the signatures the question forks.

1. **(Not blocked.)** Document the packaging hazard next to §2.3's layout: the
   2 MiB alignment gap is sparse; raw `tar` inflates 3.49× and wants `--sparse`,
   `rsync` wants `-S`, `cp` needs no flag, and compression removes the hazard.
   — verified by the measured table above, regenerated in the doc against
   `du -s --block-size=1` on a generated corpus. **Do not restate the earlier
   "`cp --sparse=always`" advice; it names a tool that is not a hazard.**
2. **(Blocked.)** `tf_tree.freeze_from_arrays(frames, edges, stamps, poses, path,
   *, layout=…)`, one episode per call, §3.3's day-one entry point. — verified by a
   test that freezes from arrays, reopens with `open_file`, and asserts `plan.at` is
   bit-identical to the same arrays pushed into a live tree (the §12 three-way
   bit-identity property, restricted to two ways); **and** by a second test that the
   per-edge ring capacities differ when the per-edge sample counts differ, which is
   the capability that distinguishes this call from `build` + `push_many` + `freeze`
   and therefore from a §6 "second spelling". If that second test cannot be written,
   this step does not have a justification and should not land.
3. **(Blocked.)** `iter_edge` / `iter_edges` / `frame_path` on both live and frozen
   trees, episode-scoped, with `iter_edge` yielding **stored** samples per §8.3's
   NORMATIVE clause. — verified by a fixture test asserting `iter_edge` returns the
   exact stamps pushed and *not* an interpolated grid, plus a `frame_path` case on a
   depth-5 chain. Closes a §13 checklist box.
4. **(Blocked.)** `PHASE5.md` §2.5 and §4 gain a short NORMATIVE paragraph: one
   `.tft` is one episode; corpus identity is external. — verified by review against
   this record.
5. **(Not blocked *in principle*, but see the note under "What would make this
   ready" — it should not land before question 2 is answered.)** §4.3's dataloader
   pattern gains a **constant-size LRU of open handles**, with the numbers that
   justify the cap (64 KiB resident per held-open file, `OSError(24)` at 1021 under
   the default `RLIMIT_NOFILE`, ~391 MiB and 6250 fds per worker at N = 10⁵ / W = 16
   if the cap is a *shard* rather than a constant). — verified by the docstring
   naming the numbers and by a test that iterates a generated corpus **larger than
   the cap** under a lowered `RLIMIT_NOFILE` and does not raise. A test that opens
   only a shard would pass without exercising the thing the cap exists for.
6. **(Not blocked.)** A record for the 48-byte store, carrying as its motivating
   cases the two confirmed reproductions above — the `frames()` round trip that
   resolves to a different frame, and `edges()` returning a self-loop — and
   explicitly **not** the refuted stronger claim that a full-name `lookup` resolves
   to the truncated frame. — verified by the record existing with a runnable
   reproduction, not by code. **Landed as
   [`0027`](./0027-the-48-byte-frame-name-store.md)**, which re-ran both
   reproductions independently rather than inheriting them and decides for a
   refusal at `intern`.
7. **(Not blocked.)** A `PHASE5.md` §12 amendment or a record attaching the worker
   language and start method to gate 4's 1.024×, with the `p` table and the
   `S ≥ 74p` minimums. — verified by the amendment quoting the measured p values and
   the spawned-Python failure at 1.248×. **Landed as an amendment inside
   `PHASE5.md` §12 gate 4**, on the argument that nothing is being decided — the
   criterion, the harness and the 1.024× all stand — so the qualification belongs
   beside the number it qualifies. It re-measured the whole table on gate 4's
   *own* fixture rather than on this corpus: spawned-Python `p` = 13.44 MiB there,
   and the gate's own file fails its own criterion at **1.785×** with a Python
   worker. Also resolves this record's open question 3.

## What would make this ready

The status is `draft` and this record does not authorise any blocked step. It moves
to `ready` when the five open questions below are answered — the first two by a
decision, the third by where the gate-4 finding lands, the fourth by looking for a
requester and not finding one, the fifth by one measurement.

**Two process notes, because a draft that authorises four of its seven steps is
worth a second look.** The README's own row for [`0013`](./0013-the-benchmark-gate-never-interpolated.md)
flags implementing from a `draft` as a process irregularity; this record does it
deliberately and says so, which is better than doing it quietly, but it is the same
shape. And **step 5 should not be treated as unblocked**: its mitigation was wrong
in the first revision of this record (a shard is not a bound) and the corrected
version rests on open question 2, which has not been measured. Steps 1, 6 and 7 are
genuinely independent of the fork this record decides; step 5 is not.

## Open questions

1. **Does the corpus index belong in this repository?** The recommendation is that
   it does not, beyond `tf_tree ingest` writing one row per episode as it freezes.
   The counter-argument is that every user will write the same 40 lines and half
   will forget the `source_digest`. Not answered here because it is a product
   scoping question, not an architecture one.
2. **Open per item, or hold a bounded cache — and how big?** Measured, an open
   costs 25.0 µs warm and 573.3 µs cold on this host (host-bound) and a held-open
   file costs 64 KiB resident (host-independent in kind). That arithmetic favours
   *some* holding, and the decision above says a constant-size LRU rather than a
   shard — but **the constant is not derived from anything measured here**, and
   **no torch `DataLoader` was in the loop**. The fork and spawn arms are raw
   `os.fork` and `subprocess`, chosen to bracket what a `DataLoader` does; which of
   the two a given torch version picks on a given Python was not checked. One
   measurement with a real `DataLoader`, sweeping cache size against epoch time and
   resident bytes, closes this and sets the constant. Until it runs, step 5's
   docstring can state the costs but should not state a recommended cap.
3. **Where does the gate-4 scope finding land** — a §12 amendment, or its own
   record? It is a bigger claim than this one and burying it here would be the
   wrong home for it.

   **Resolved: a `PHASE5.md` §12 amendment**, landed with step 7. Size is not the
   criterion for a record; deciding something is. Nothing about gate 4 is being
   decided — the criterion stands, the harness is right, and 1.024× reproduced on
   re-measurement — so what the finding adds is scope to an existing number, and a
   qualification that does not sit beside the number it qualifies does not do its
   job. `0023` and `0025` are records because each *changed* a gate; a proposal to
   give criterion 4 a second worker arm would be one too, and the amendment names
   that as the thing it does not do.
4. **Is there a requester for a single-file corpus?** There is none today. If one
   appears, this record reopens — and the argument has to start by refuting the
   **peak-RSS** arithmetic (~0.6 MB/episode measured, ~60 GB extrapolated at 10⁵)
   and the O(N) update-and-delete cost, because the Pss table gives it nothing to
   stand on and the *wall-clock* rebuild rows do not favour per-episode either.
5. **What does a variable-length corpus do to the byte totals?** Every episode
   measured here is identical — 10.24 s, 7212 samples — and `tf_tree.build` takes
   one uniform ring capacity, so both shapes overpay by the same common-mode factor
   and every ratio above is unaffected. A real corpus's episode lengths vary by an
   order of magnitude, and per-episode sizing (§3.1, and step 2's
   `freeze_from_arrays`) is where that stops being common-mode. Nothing in this
   record's decision turns on it, but the *absolute* byte figures are not corpus
   figures until it is measured.

## Instrument check and provenance

**The bulk of the numbers above come from a dedicated measurement pass on this
host, with its commands quoted inline.** Its own instrument checks were: the wheel
under test (`transform_tree` 0.0.2, `/tmp/sdvenv`) reports `FORMAT_VERSION = 3` and
`layout_hash = 0x3d104195`, matching the repository, and its mtime postdates the
last commit touching `crates/tf_tree_py` or `crates/tf_tree_core`, so it is not a
stale artifact; a smoke test confirmed frozen and live `plan.at` are bit-identical,
so the read path under measurement is the real one; and every Pss arm carries a
no-touch control that fails loudly (8.87× and 3.37×), so no arm passes vacuously.

**Four claims were re-run independently while writing this record**, because they
are the ones the decision turns on and inheriting them would have been the third
failure in this project's list:

```
$ /tmp/sdvenv/bin/python -c "import tf_tree; print(tf_tree.arena_format_version(), hex(tf_tree.arena_layout_hash()))"
3 0x3d104195
$ du -sh --apparent-size A ; du -sh A ; filefrag -v A/ep000000.tft
2.8G / 801M / extent 0 = 1 block at logical 0, extent 1 from logical 512
$ python  # prefix budget sweep            -> cliff between 28 B and 29 B, as above
$ python  # frames() round-trip collision  -> -99.0 from the copied-out name
$ python  # fd/VMA/Rss after one open_file -> delta fds=1 maps=1, Rss 64 kB
```

### Review pass — what a second party re-ran, and what it changed

A reviewer re-measured seven claims on the same host and corpus, from scripts
written independently of the measurement pass where the claim was a correctness
one. **Five reproduced exactly, one was arithmetic that its own inputs do not
support, and one was refuted in its stated form.**

| claim | this record said | re-measured | verdict |
|---|---|---|---|
| bytes, apparent / on disk | 2 930 816 000 / 839 680 000 | identical, `filefrag` hole identical | reproduced |
| Pss, spawned, shape B, W=16/W=1 | 1.2480 | 1 024 081 / 820 423 = **1.2482** | reproduced |
| Pss, spawned, shape A, W=16/W=1 | 1.2509 | 1 052 725 / 841 440 = **1.2511** | reproduced |
| one `open_file` | +1 fd, +1 VMA, 64 kB Rss | identical, VMA offset `00200000` | reproduced |
| fd ceiling at soft `RLIMIT_NOFILE` = 1024 | 1021 then `OSError(24)` | identical | reproduced |
| prefix budget | cliff at total length 49, budget 28 B | identical, independent script | reproduced |
| build rate, N = 100 | A 3.26 ms/ep, B 2.7 ms/ep, 91 508 kB | A 0.32 s, B 0.26 s, 91 392 kB | reproduced |
| packaging inflation | "3.6×" | 2 930 816 000/839 680 000 = **3.4904**; `tar` measured **3.4915×** | **corrected** |
| `cp --sparse=always` needed | listed as a remedy | `cp` default preserves the hole (83 972 096 vs 83 968 000) | **withdrawn** |
| full-name `lookup` under truncation | implied to answer wrongly | `lookup('root', V)` returns **−104.0, correct** | **refuted** |

The refutation is the one that matters, and it is written into item 4 above rather
than smoothed over: **the engine never confuses two frames sharing a 48-byte
prefix.** The surviving finding is narrower and is about `frames()`/`edges()` output
being unusable as input, which is enough for the decision but is not the same claim.
Two hazards the measurement pass did not look for were found in the same pass and
added as item 5 and the two paragraphs after it: `edges()` reporting a **self-loop**,
and `len(name) == 48` having false positives as a tell.

Reviewer commands, for regeneration:

```
$ python rev_trunc.py    # prefix sweep, V/T collision, manifest grep, self-loop
$ python rev_trunc2.py   # edges() under a 56-byte key, UTF-8 split, 48-byte genuine name
$ python m_pss.py b B.tft 1 1024 1 all ; python m_pss.py b B.tft 16 1024 1 all
$ python m_pss.py a A   1 1024 1 all ; python m_pss.py a A   16 1024 1 all
$ python m_limits.py A/ep000000.tft 1024 5000
$ tar -cf plain.tar -C A $(ls A | head -100) ; tar --sparse -cf sparse.tar ... ; cp ... ; rsync ...
```

All four reproduced. The code facts they rest on were read rather than assumed:
`frame.rs:151` (`let n = src.len().min(48)`, no refusal), `frame.rs:102–108`
(`blake3_64` over the full name) and `frozen.rs`'s `read_manifest` plus the
`OwnedFd` field whose doc comment states the fd is retained for it.

**Not verified, and stated as such:** every 10⁵ figure is a linear extrapolation
from measured N = 100/250/500/1000 points — shape B at 10⁵ could not be built here
for the memory reason it is cited for. The `vm.max_map_count` ceiling is arithmetic
from the measured +1 VMA per open; lowering the sysctl to observe the failure needs
root. `fault_around_bytes` is not readable on this host, so 64 KiB is a measured
`Rss` with a consistent explanation attached, not a read knob. And the absolute
latencies throughout — µs per open, ns per stamp, seconds per rebuild — are
host-bound on a contended 4-physical-core guest and per §9.3 must be marked
UNAVAILABLE rather than promoted into a gate row.
