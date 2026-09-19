# 0036: the receipt time the format already reserved

**Status:** implemented
**Owner:** @NoeFontana

**Implementation:** **all five steps have landed** — #272 (steps 1–2), #273
(step 1's amendment: the field stores `wall clock - stamp` rather than the wall
clock, and is called `ClaimRecord::clock_offset_nanos` rather than
`last_push_nanos`), and #274 (steps 3–4). Step 5's cost is in `docs/PHASE1.md`
§11.2 with its disclosure. **This record is now frozen**: drift is fixed by a
record that supersedes it, not by editing here.


## Context

`PHASE2.md` §6.4 says `heartbeat` is bumped on every push; a wall-clock read costs
~38 ns (a monotonic one 28 ns) against a ~5 ns push. The arena format already
reserved a per-claim field, so populating it engages no format decision:
`ClaimRecord::clock_offset_nanos`. `TFT004` (`PHASE5.md` §6) needs it.

## Decision

**Sample the offset, once per second of published data, in the `tf_tree` facade.**

- `EdgeWriter` gains `sample_every: u32` (`nominal_rate_mhz / 1000`, clamped to at
  least 1; a fixed default where the edge declares no rate) and
  `since_sample: Cell<u32>`, both set in `Tree::claim`. `tf_tree_core` does not
  change (it is `no_std` and cannot read a clock, D14).
- `EdgeWriter::push` calls `self.publisher.push(stamp, iso)?` **first**, then
  counts down, and on the sample does one `Relaxed` store of
  `now_nanos() - stamp` into `claim.clock_offset_nanos`. The `?` is load-bearing:
  a failed push records nothing and does not spend the interval.
- **A claim's first push samples, and a claim clears the receipt it inherits**
  (`Tree::claim` stores `0`), so a replacement writer never publishes under its
  predecessor's offset.
- **Only the wall-clock domain samples**; `sample_interval` returns `0` for every
  other domain tag.
- The store is a **difference**, never the clock reading (see the section below).
- `Cell<u32>` keeps `OwnedWriter` `Send + !Sync`; no `unsafe impl`.

## The receipt time could not be paired with a stamp

The receipt belongs to the last *sampled* push while the ring's newest stamp
belongs to whatever was published since, so `receipt - newest_stamp` swings by one
sampling interval (+3 us to -900 ms on an exact 10 Hz clock). The field therefore
stores `wall clock - stamp` at the sampled push, and step 3 reads it as the offset
without computing one.

## Open questions

1. **Fixed interval, or derived from `nominal_rate_mhz`? Derived**, one offset per
   second of published data, one division per claim, not per push.
2. **Which clock? The host wall clock in integer nanoseconds** (stamps are
   wall-clock-domain, `API.md` R3); inherit `TFT005`'s epoch skip via `Clock` in
   `tf_tree_cli`; skip on a frozen `.tft` source.
3. **What does `doctor` do with one offset per edge? Report the spread and flag
   nothing until a threshold has evidence.** One sample cannot separate clock error
   from stamp-to-push latency, which differs legitimately between publishers, so a
   fleet-relative rule would report a healthy difference as skew.
4. **Is a 1-in-`INTERVAL` clock read acceptable on the publish path? Yes, with one
   constraint:** the clock read sits outside the seqlock window (after
   `publisher.push` returns), because a longer write window is more
   `SlotContended` retries for every reader. The p99.9 effect for a 1 kHz publisher
   is arithmetic, unmeasured, and stated in `PHASE1.md` §11.2.

## Consequences

- The sampler costs +1.1 ns per push at 1024-push intervals; cost per second of
  publishing is `38.4 + rate x 1.06` ns (`just push-sampler-cost`,
  `crates/tf_tree_bench/benches/push_sampler.rs`, tabulated in `PHASE1.md` §11.2).
- Sampling off the edge's `heartbeat & mask` was built and is slower (+1.4 ns) and
  cannot sample a new writer's first push.

## Implementation plan

1. **The sampler, entirely in `tf_tree`.** Tested in
   `crates/tf_tree/tests/clock_offset.rs` (sampling schedule and skew injection, a
   failed push records nothing, no-rate edge samples at the default, a fresh claim
   starts a fresh interval and does not inherit an offset, the offset does not move
   with the newest stamp, non-wall-clock domains record nothing) and unit tests in
   `tree.rs` (`recorded_offset` never returns the `0` sentinel;
   `sample_interval`). Assertions detect the store by zeroing the field, never by
   comparing two readings.
2. **`PHASE2.md` §6.4's amendment**: the sampled rule, with *never a reaping
   trigger* restated.
3. **`TFT004` itself.** Per edge: `clock_offset_nanos` **is** the offset; report
   the fleet spread; flag nothing. Four skips, each with its own reason string:
   nothing sampled yet (`clock_offset_nanos == 0`), `TFT005`'s epoch condition, a
   frozen `.tft` source, and **a replayed source** (`tf_tree_ingest` publishes
   through `EdgeWriter::push`, so replay stamps present-day receipts against old
   stamps; the ingest path marks it via `PushStream::no_live_receipt`).
4. **`PHASE5.md` §0.0**: seventeen detecting; `TFT004` out of the "cannot detect
   anything in any configuration" group.
5. **The cost**, on the record in `docs/PHASE1.md` §11.2 with its disclosure.
