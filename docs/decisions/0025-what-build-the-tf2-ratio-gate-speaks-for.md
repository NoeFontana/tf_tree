# 0025: what build the tf2 ratio gate speaks for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** the measurement half is already in the tree
(`just tf2-ratio-profiles`, `ratio.rs`'s two estimate constants and the test that
pins both relations). What this record adds is prose: the gated row states which
build it speaks for. **`FLOOR` does not move.**

**The decision changed between draft and ready, because the deciding question was
measured rather than argued.** The draft weighed "state that the gate claims the
LTO build" (A) against "add a second gated row at `[profile.embedder]`" (B) and
recommended A weakly. Repeating the paired measurement three times killed B on
evidence: at a consumer's build **this host cannot resolve the row at all** — see
*The three repeats* — so a second gated row would be a threshold set from a band
that straddles it. A is now the recommendation for a stated reason instead of a
preference.

## Context

`lookup_ratio_vs_tf2` is the project's headline gated row: tf_tree's depth-3
lookup against `tf2::BufferCore`'s, as a paired median-of-quotients, floored at
`FLOOR = 2.0`.

`ratio.rs` guards that floor with a compile-time assertion —
`assert!(FLOOR < UNBIASED_ESTIMATE)` — whose job is to stop the gate from being
passable on measurement bias alone. `UNBIASED_ESTIMATE = 2.25` is the ratio with
every known bias removed, so a floor beneath it is a floor a *biased* run cannot
clear on bias alone. That is the property the assertion exists to hold.

**The assertion holds for this workspace's build and not for a consumer's**, and
the difference is large enough to matter:

| build | LTO | tf_tree | tf2 | paired ratio |
|---|---|---|---|---|
| workspace `release` | `thin` | 201.55 ns | 504.44 ns | **2.4900×** |
| `[profile.embedder]` | `false` | 244.15 ns | 506.14 ns | **2.0745×** |

`[profile.embedder]` is not a contrivance: it is cargo's release defaults written
out. Cargo applies the **top-level package's** profile to the whole dependency
graph, so somebody who runs `cargo add tf_tree` and builds in release gets the
`lto = false` row, not the workspace one.

The paired ratio at that build is 2.0745× and still clears 2.0. **The unbiased
estimate does not.** With the binding bias removed as well,
`UNBIASED_ESTIMATE_DEFAULT_RELEASE ≈ 1.80×` — below the floor. So at a consumer's
build the gated row passes, but it passes *on binding bias*, which is precisely
the thing `assert!(FLOOR < UNBIASED_ESTIMATE)` was written to make impossible.

**The cross-profile read is legitimate, and that was checked rather than
assumed.** Across the profile change the tf2 column moves **+0.34%** while the
tf_tree column moves **+21.1%** — the control predicted in advance, since an
`extern "C"` call into a C++ shim cannot be inlined by any Rust LTO setting. A
tf2 column that had moved with the profile would have meant the two runs were not
comparable and this record would not exist.

## The three repeats, which decided it

The draft rested on one pair of runs. Three more, same container, same
`taskset -c 2`, 9 rounds x 10240 lookups per arm:

| run | workspace `release` | verdict | `[profile.embedder]` | verdict |
|---|---|---|---|---|
| 1 | 2.4869x, band 2.464-2.777 | ABOVE | 2.0884x, band 2.080-2.137 | ABOVE |
| 2 | 2.4439x, band 2.430-2.462 | ABOVE | 2.0833x, band **1.975**-2.563 | **UNRESOLVED** |
| 3 | 2.5073x, band 2.446-2.856 | ABOVE | 2.0474x, band **1.923**-2.340 | **UNRESOLVED** |

**Two findings, and they point opposite ways.**

**The medians are stable.** Workspace 2.444-2.507, consumer 2.047-2.088 — about
2% spread on each, across three independent runs. So question 2's worry is
answered: 2.07x is not one lucky run, and neither is the ~1.80x that follows from
it once the binding bias is removed. The *number* is real.

**The consumer build's verdict is not resolvable on this host.** In two of three
runs the band straddles 2.0 and `ratio.rs` says so — `UNRESOLVED`, not `ABOVE`,
because `verdict` compares the **band** against `FLOOR` and not the median. That
is the gate behaving exactly as designed, and it is the design that decides this
record: a row whose band crosses its own floor two runs in three is not a row to
hang a second threshold on.

Note which arm is noisy. The workspace band is 1.3-16.7% wide and the consumer
band 2.7-29.8%; the consumer arm is *both* closer to the floor and wider. Nothing
here separates that from host noise, and this host has no quiet mode to check it
against.

## The decision

**Option A: `FLOOR` stays at 2.0, the gate keeps speaking for the workspace
build, and the row says so in as many words.** No second gated row, no change to
any constant.

The row's prose becomes, in substance: *this gate speaks for a build with
`lto = "thin"`, which is what this workspace ships. A consumer who builds at
cargo's release defaults — which is what `cargo add tf_tree` and
`cargo install tf_tree_cli` both produce — measures about 2.07x, and this host
cannot resolve that against the 2.0 floor: two runs in three come back
UNRESOLVED. With the binding bias removed as well the estimate is ~1.80x, under
the floor. This gate does not speak for that build.*

### Why not B, now that question 1 made B look stronger

Question 1's answer — that `[profile.embedder]` *is* a user's build on both knobs
that matter — is a real argument for gating it, and in the draft it was the
strongest one. **The repeats overrule it on a point of method, not of
preference.** B requires choosing a floor for the consumer row. The only
available basis is measurement on this host, and this host returns UNRESOLVED for
that row in the majority of runs. Setting a threshold from a band that contains
the threshold is the precise failure `0023` question 4 flags for R1's provisional
1.10, and doing it knowingly, one record later, would be worse than doing it by
accident.

A floor low enough to pass reliably — under 1.80, as the draft noted — would then
be a number chosen to be passed rather than derived, which is the sample-selection
move this project has refused repeatedly. **A gate that always passes is not a
weaker gate than none; it is worse, because it reads as evidence.**

### What A costs, stated plainly

The build this gate declines to speak for is every build outside this repository.
That is not a small exclusion and A does not make it smaller — it makes it
*stated*. The honest position is that tf_tree is measured at 2.49x in the
configuration the project builds and ships from, around 2.07x in the
configuration a dependent gets, and that the second of those is not gateable
here. All three of those facts are now in the artifact.

**B is not foreclosed and its precondition is now written down**: a host that
returns a resolved verdict for the consumer row across repeated runs. On such a
host, re-run `just tf2-ratio-profiles`, and if the consumer band clears 2.0
consistently, B becomes a threshold derived rather than chosen. Until then the
sentence A adds is the sentence B would replace.

## What is NOT open

- **`FLOOR` does not move down.** Under either option.
- **The `assert!(FLOOR < UNBIASED_ESTIMATE)` pin stays**, as does the test
  `the_floor_is_bounded_at_one_profile_and_not_the_other`, which asserts *both*
  `FLOOR < UNBIASED_ESTIMATE` **and** `FLOOR > UNBIASED_ESTIMATE_DEFAULT_RELEASE`
  — the second of those is the deficiency this record is about, pinned so it
  cannot be quietly resolved by editing a constant. Its failure message says not
  to delete it, and that stands.
- **Which number is quoted in prose.** `UNBIASED_ESTIMATE_DEFAULT_RELEASE` is
  `pub` and `UNBIASED_ESTIMATE` is private on purpose: the weaker figure is the
  one a reader can reach.

## Open questions

1. ~~**Does `cargo install tf_tree_cli` get the workspace profile?**~~
   **Answered: no, and the first draft of Option A asserted the opposite.**
   `[profile.*]` is honoured only in a *workspace root* manifest, and
   `crates/tf_tree_cli/Cargo.toml` declares no profiles of its own — the
   workspace root does. A crate installed from the registry is unpacked as its
   own root, so `cargo install tf_tree_cli` builds at cargo's release defaults:
   `lto = false`, `codegen-units = 16`.

   **That is `[profile.embedder]` exactly** — `Cargo.toml:185-189` sets those two
   values and inherits the rest. So the "consumer" arm is not a stand-in for what
   a user gets; it *is* what a user gets, whether they `cargo add tf_tree` or
   `cargo install tf_tree_cli`. The workspace's 2.49× is reachable only by
   building inside this repository.
2. ~~**Is 1.80 stable, or is it one host's number?**~~ **Answered: stable as a
   number, unresolvable as a verdict** — see *The three repeats*. Three runs put
   the consumer median in 2.047-2.088 (2% spread), so the figure repeats; but the
   *band* straddles 2.0 in two of the three, so the row cannot be gated here.
   This is what decided against Option B, and it replaces the paragraph below,
   which was written when the answer was unknown.

   Superseded text: it comes from a single pair of
   profile runs. The tf2-column control makes the *comparison* sound; it does not
   make the value repeatable. Before B could set a floor from it, it needs the
   same paired treatment the 2.49 got.
3. ~~**Does the embedder-build gap survive `codegen-units = 1`?**~~ **Moot, by
   question 1's answer.** The worry was that `[profile.embedder]` might be
   pessimistic relative to a consumer's real build, since `lto = false` is not
   the only knob governing cross-crate inlining. It is not pessimistic: the
   profile sets `codegen-units = 16`, which is cargo's release default, so both
   knobs already match what a consumer gets. There is no more favourable
   realistic build to hope for.
