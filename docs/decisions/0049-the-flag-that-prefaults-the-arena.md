# 0049: the flag that prefaults the arena

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** (filled in as work lands)

## Context

`docs/PHASE2.md` §7.4 specifies a memory-locking knob:

> `LockPolicy::{ None, Populate (default), Locked }`. `Locked` calls
> `mlock2(MLOCK_ONFAULT)` and requires `RLIMIT_MEMLOCK` […]

It has never been implemented — `git grep -n LockPolicy -- .` finds it only in
prose, with no type and no call anywhere under `crates/` — and its body has never
been amended. `docs/API.md` §8.3
and `PHASE2.md` §0.0's row both declined it, and **they agree with each other**;
the pair is not a contradiction to adjudicate. What is stale is §7.4's own body,
which still reads as a live requirement while both of the documents that
describe it say it will not be built.

`git log -L` on either range resolves to a single commit, but that is a
squash-merge artifact and the propagation is the more useful fact. The three
spellings were authored in order on one branch: `docs/API.md` §8.3 first, then
the same sentences into `crates/tf_tree_cli/src/checks.rs`'s operator-facing
`TFT016` finding and `hostfacts.rs`'s module doc, then `PHASE2.md` §0.0's status
row. **A documentation error reached shipped code and a status table inside one
PR**, and nothing between the three could disagree, because each was copied from
the one before.

Nothing in `docs/decisions/` covers memory locking: the grep for
`mlock|lockpolicy|memlock` over that directory returns only `0012`'s
`ClockPolicy`. So the decline has been in force with no record behind it, which
is what this one is.

**§8.3 asserted two syscall behaviours and reproduced no probe**, against
`PHASE2.md`'s own preamble rule that where a syscall behaviour is asserted the
probe is reproduced. §8.4 already confesses the class — *"'No syscall' and 'no
lock' are read from the code, not enforced by a test"* — and this is that
confession's first casualty.

## Decision

**No `LockPolicy`. No `mlock` call of any kind in this library. §7.4 stays
declined, and the outcome does not change.** What changes is the reason, one
recommended flag, and one operator-facing message.

### The decline rests on §8.3's second bullet, which survives everything

A library that locks memory on its caller's behalf is spending an
`RLIMIT_MEMLOCK` budget it cannot see, and the arena is deliberately
over-provisioned (§3.8), so the process that knows how much of it this node will
touch is the embedding application. That argument alone carries the decline. It
is about **who decides**, not about what the flag does.

### §8.3's third bullet is corrected, clause by clause, at three different strengths

Measured on this host — Linux 6.8.0-138-generic, `/proc/swaps` header-only,
`free -m` Swap 0 — with `crates/tf_tree_bench/examples/mlock_probe.rs`, which
this record commits. Every reading below is a dated output of a named arm, not a
live claim.

| clause | verdict |
|---|---|
| *"`MLOCK_ONFAULT` would not prefault"* | **TRUE** |
| *"so it adds nothing over §7.1"* | **FALSE as a mechanism claim** |
| *"on a swapless host the pages are not reclaimable anyway"* | **UNDETERMINED** |

**It does not prefault** (`mlock_probe onfault`): `mlock2(MLOCK_ONFAULT)` on an
untouched 64 MiB `memfd` mapped `MAP_SHARED` returns 0 with `Rss=0 kB`. That is
why it is the *right* flag for an over-provisioned arena rather than a useless
one.

**Population and retention are different things** (`mlock_probe retention`).
§7.1 establishes PTEs once; `MLOCK_ONFAULT` is what keeps them. On one mapping
with `VM_LOCKED` as the only variable: `MADV_POPULATE_WRITE` gives `Rss=65 536
kB`; `mlock2(MLOCK_ONFAULT)` returns 0; `MADV_PAGEOUT` then returns `-1 EINVAL`
with `Rss` unchanged; `munlock` returns 0; and **the same `MADV_PAGEOUT` on the
same range** then returns 0 with `Rss=0 kB`.

**Whether that matters is UNDETERMINED, and this is the clause to read
carefully.** `MADV_PAGEOUT` is a *directed* reclaim: it isolates the named
folios by address and forces the same `shrink_folio_list()` the LRU scanner
runs. It establishes that the teardown mechanism is not blocked by
swaplessness. It does **not** establish that a kernel under organic pressure
would select these folios — and measured, on this host, it does not.
`mlock_probe pressure` under `systemd-run --user --scope -p MemoryMax=96M`,
3 runs: the arena's `Rss` stays at **65 536 kB at every 4 MiB anon step** and
the process is **SIGKILLed at 28 MiB of anon growth**. The positive control is
the same probe, same cgroup, same limit, with the `memfd` swapped for a
file-backed mapping — `mlock_probe pressure-file`, 3 runs: the arena's `Rss`
falls **65 536 → 3 072 kB** and the process reaches **92 MiB** of anon before it
dies. So the instrument can see organic PTE teardown, and it does not see it for
shmem: the kernel OOM-killed instead.

**Global `kswapd` pressure is untested and is the residual.** Filling 31 GiB on
a shared host to find out was judged not worth it, and a memcg is not the same
scanner. So the honest verdict on §8.3's swapless clause is *undetermined*, and
this record does **not** claim it is false.

**What §8.3 actually got wrong there is the grammar, and that is a defect
whatever the answer turns out to be.** *"On a swapless host the pages are not
reclaimable anyway"* is a conclusion about reclaim **policy** stated in the form
of a **mechanism fact**, in a document a consumer reads as a contract. A reader
cannot tell from it that it depends on which scanner runs, on whether the
mapping is shmem, and on the host's cgroup configuration — none of which the
sentence mentions. It is corrected to say what is measured and what is not.

### The recommended incantation is wrong, and it is the clause an operator acts on

§8.3 recommends `mlockall(MCL_CURRENT | MCL_FUTURE)`. **Measured
(`mlock_probe mlockall`): it prefaults.** A fresh untouched 64 MiB `memfd`
mapping goes `Rss=0 kB` → **65 536 kB** the instant the caller issues it, and a
mapping created *after* the call comes up at **65 536 kB** immediately —
`MCL_FUTURE` is not only about future locks.

That is per-arena population at whole-address-space granularity: the thing
§7.1's own title forbids, and the thing
[`0024`](./0024-population-is-per-edge-at-take-up.md) (`ready`) removed at a
measured **5.2×** (38 248 448 B → 7 368 704 B charged, 101% of the arena → 19.5%).
**§8.3 told every embedder to undo `0024`.**

The correct spelling is `mlockall(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT)`
(`mlock_probe mlockall-onfault`): untouched `Rss=0 kB`, a new mapping after the
call also `Rss=0 kB`, and once touched, `Rss=65 536 kB` with `MADV_PAGEOUT`
refused. `MCL_ONFAULT` is `MLOCK_ONFAULT` at address-space scope — the
address-space spelling of the very flag §8.3 dismissed.

### `TFT016`'s message stops predicting `mlockall`'s outcome

The check compares `RLIMIT_MEMLOCK` against the arena size and its message said
*"a consumer that pins its address space with mlockall(MCL_CURRENT|MCL_FUTURE)
will fail"*. **The comparison does not predict that call, and it is not
conservative** — `mlockall` charges the process's **whole address space**, not
the arena.

Measured (`mlock_probe memlock-limit`) against the doctor fixture's 23 424-byte
arena: at `RLIMIT_MEMLOCK` 65 536 and 1 048 576 bytes `mlockall` returns
`-1 ENOMEM` **under both flag sets**, and succeeds at 8 388 608. Driven through
the shipped binary, `ulimit -l 64` (65 536 bytes, comfortably above the arena)
emits **no memlock finding at all**, while `ulimit -l 8` emits one. So `TFT016`
is silent on hosts where the advice it prints fails.

**The comparison itself is kept and the message is narrowed.** The limit being
below the arena is still a real finding — the arena alone cannot be pinned — and
it is the only term this check can compute: `tf_tree_cli` is
`#![forbid(unsafe_code)]` with no `libc`, so it cannot call `getrlimit`, and the
address space of a future consumer process is not knowable from a doctor run
against an arena. What the message must stop doing is **predicting a call whose
other term it cannot see**, and what it must start saying is that its silence is
not a clearance.

## Rationale

### Why the decline survives the finding that `MLOCK_ONFAULT` does something

The tempting move, once `retention` shows that locking really does retain what
§7.1 populated, is to build §7.4. It does not follow. Everything measured here
is about **what the flag does**; §8.3's second bullet is about **who may spend
the budget**, and nothing above touches it. Implementing `LockPolicy` would also
put an OS-boundary `unsafe` call inside the engine, which
[`0007`](./0007-the-unsafe-budget-and-the-c-abi.md) constrains and which no part
of this record argues for.

### Why the probe is committed, and why it is an example rather than a recipe

§8.3 survived being wrong because nobody could re-run it. A corrected §8.3 with
no executor is the same artifact with better numbers in it.

**It is a cargo example**, in `crates/tf_tree_bench/examples/`, and that is
load-bearing rather than incidental. `scripts/evidence-audit.sh` derives its
entire subject set from `cargo metadata --no-deps`, `kind in (example, bin,
bench)`, and requires a backticked row in `docs/benchmarks/EVIDENCE.md` for
every target in it that no recipe executes. **A fenced block in `PHASE2.md`
Appendix B is not in that set**, so registering one would be a row nothing reads
and an audit that is green either way — the shape of a check that cannot fail.
An example is in the set, so the row is *required* and the audit fails without
it.

**It is registered as a probe and not run by a recipe**, because every arm's
answer is a property of the running kernel, the cgroup and whether the host has
swap. A recipe asserting any of them would be a gate about the machine.
`EVIDENCE.md`'s own taxonomy already draws that line and says a probe is not
second-class: what it owes is a documented command and prose that reads as *was
measured*.

### Why it may carry `unsafe` at all

`crates/tf_tree_bench/src/lib.rs` is `#![forbid(unsafe_code)]`; an example is a
**separate crate root**, so that attribute governs none of the file, and the
probe declares its own posture explicitly rather than relying on the default.
Its `mmap`/`mlock2`/`mlockall`/`madvise` calls are `0007` rule 1's **kind 2, the
OS** — the same kind as `tf_tree_ipc`'s, in a crate rule 1's parenthetical does
not name. [`0048`](./0048-a-kind-is-not-a-crate-name.md) is what makes that a
statement rather than a loophole: it rewrites rule 1 so the kinds are properties
and the crate names are a non-normative index. **This record depends on `0048`
and should not land ahead of it.**

### Why `TFT016` does not grow an address-space term

It cannot compute one. `getrlimit(2)` needs `libc` and `unsafe` in a
`#![forbid(unsafe_code)]` crate, `/proc/self/limits` reports *this* process's
limit rather than the consumer's, and the consumer's address space is a property
of a program the doctor has never seen. Reporting the arena's own share and
saying plainly that it is one term of two is the whole of what is available.

### Why the warm/cold comparator is not published as a ratio

`mlock_probe refault-cost` reads a warm pass of **156.4 µs** and a cold re-read
of **8 436.5 µs** over 16 384 pages, at 1 024 minor faults and **0 major
faults** — the folio surviving in page cache with no swap on the host, while the
mapping did not. That is ~8.1 µs per fault event, which is `PHASE2.md` §7.1's
own *"single-digit microseconds"* arriving **after** §7.1 has run, and it is the
figure worth carrying.

**The ratio is not.** An earlier reading of the same comparison put the warm
pass at 4.5 µs, which is 0.27 ns per dependent load and impossible: at `-O2` a
warm-read loop whose accumulator is discarded compiles away entirely. This
probe's loops read through `read_volatile` and print the accumulator, so they
cannot be eliminated. A ratio taken against a loop that did not run is a
measurement of the optimiser.

## Consequences

- **`docs/API.md` §8.3's recommendation changes in every place it is spelled,
  including the shipped operator string.** Correcting the two documents and
  leaving `checks.rs` would be the worst outcome: the code is what an operator
  acts on, and `TFT016` has already been corrected once for giving bad advice.
- **`TFT016`'s comparison is unchanged; its message is narrower and now
  discloses that its silence is not a clearance.** The check gets quieter in no
  configuration — the firing condition is identical.
- **`docs/PHASE2.md` §7.4 gains an amendment banner** and stops reading as a
  live requirement.
- **`docs/PHASE5.md` §6's `TFT016` row said the fact comes from `getrlimit`.**
  It does not: `crates/tf_tree_cli/src/hostfacts.rs` text-parses
  `/proc/self/limits`, for the reason its own header gives. Corrected with the
  message change, because a spec row naming a syscall this crate cannot call is
  the same class of defect as the message that named the wrong flag.
- **`PHASE2.md` §0.0's row cites `checks.rs:1987`, which has drifted 466 lines**
  — it was exact when written and now lands inside an unrelated doc comment.
  It is re-cited **by name** (`fn tft016`'s `match host.memlock` arm), because a
  fresh line number restarts the same clock. **Nothing in `scripts/` guards a
  doc citation**, so the replacement is unguarded too; naming a function rather
  than a line is a mitigation and not a fix.
- **A new `unsafe`-carrying target enters the tree**, which
  [`0048`](./0048-a-kind-is-not-a-crate-name.md)'s index and gate must account
  for. It is kind 2.
- **Reopening `LockPolicy` stays out of scope.** If a future consumer needs it,
  §2's kernel floor (*"kernel >= 3.17 for `memfd_create` and `F_ADD_SEALS`, and
  >= 3.15 for OFD locks"*) sits below `mlock2`'s availability — `man 2 mlock2`'s
  HISTORY gives *"Linux 4.4, glibc 2.27"* for the call and *"since Linux 4.4"*
  for `MCL_ONFAULT`, and the glibc floor is a second constraint — so it would
  need a fallback in the shape `MADV_POPULATE_*` already has for < 5.14. Noted
  so the next reader does not rediscover it.

## Implementation plan

1. **`crates/tf_tree_bench/examples/mlock_probe.rs`**, with the arms named
   above, each `mlockall` arm in its own process because the call is
   process-wide. — verified by running it: every reading in this record is its
   output, and the two pressure arms were run 3× each with the file-backed
   positive control.
2. **`docs/benchmarks/EVIDENCE.md`** gains its probe row. — verified by
   `bash scripts/evidence-audit.sh`, and red-tested by deleting the row and
   watching it report `UNREGISTERED tf_tree_bench example mlock_probe`.
3. **`crates/tf_tree_cli/src/checks.rs`** — `TFT016`'s finding message. — verified
   by driving the shipped binary at `ulimit -l` 8 (fires) and 64 (silent) against
   the 23 424-byte fixture.
4. **`crates/tf_tree_cli/src/hostfacts.rs`** — the same correction in the module
   doc.
5. **`docs/API.md` §8.3** — the flag on bullet 2, and bullet 3 rewritten to the
   three verdicts above with the probe named. — verified by
   `git grep -n 'adds nothing over'`: every surviving hit is a **quotation of
   the withdrawn clause**, in this record, in `API.md`'s own correction, in
   `hostfacts.rs`'s, and in the probe arm named for it. No site asserts it.
6. **`docs/PHASE2.md`** — §7.4's amendment banner; §0.0's row's clause and its
   `checks.rs:1987` citation.
7. **`docs/PHASE5.md`** — §6's `TFT016` detection row (`getrlimit` →
   `/proc/self/limits`).
8. **`CHANGELOG.md`** entry. **`docs/decisions/README.md`**'s index row is
   added centrally rather than on this branch, for the reason `0047` step 8
   gives.

## Open questions

1. **Would global `kswapd` pressure tear down a shmem arena's PTEs on a swapless
   host?** Untested here: a memcg is a different scanner, and filling 31 GiB on a
   shared machine to find out was judged not worth it. Until it is answered,
   §8.3's swapless clause is **undetermined** and no document may state it either
   way. Answering it does not change the decline — §8.3's second bullet is
   independent of it — so this is open for the sake of the *sentence*, not the
   decision.
