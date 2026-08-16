# 0025: what build the tf2 ratio gate speaks for

**Status:** draft
**Owner:** @NoeFontana
**Implementation:** none. The measurement half is already in the tree
(`just tf2-ratio-profiles`, `ratio.rs`'s two estimate constants and the test that
pins both relations). **Nothing here changes `FLOOR` until this record is
ratified**, and the two options below are not equivalent — one narrows what the
project claims, the other adds a claim.

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

## The decision to be made

**Not** "lower the floor". Lowering `FLOOR` to fit the weaker build is the
sample-selection move this repository has refused several times, and it would
delete the one assertion that makes the gate mean something. The two live options
are:

### Option A — state that the gate claims the LTO build

`FLOOR = 2.0` stays, `UNBIASED_ESTIMATE = 2.25` stays, and the row's prose says
in as many words: *this gate speaks for a build with `lto = "thin"`, which is
what this workspace ships; a consumer who builds at cargo's release defaults gets
roughly 2.07× measured and ~1.80× unbiased, and this gate does not speak for that
build.*

**An earlier draft of this sentence also claimed `cargo install` produces the
workspace build. It does not** — see question 1 below, which is answered. The
sentence above is the corrected form, and the correction is most of why the
recommendation below is weaker than it was.

- **For:** it is true today, it costs nothing, and it narrows a claim rather than
  widening one. It is also the honest reading of what was actually measured — one
  gated row, one build.
- **Against:** the build it declines to speak for is the one most users will
  have. A gate that covers the flattering configuration and says so is still a
  gate that covers the flattering configuration.

### Option B — add a second gated row at `[profile.embedder]`, with its own floor

Two rows, two builds, two floors, each with its own unbiased estimate beneath it.
The second floor must sit under 1.80 to keep the assertion's property.

- **For:** it gates the build a consumer gets, which is the one the claim is
  really about. The machinery already exists: `BUILD_CRITICAL_FACTS` refuses to
  score two profiles against one baseline, so the rows cannot be confused for one
  another, and `just tf2-ratio-profiles` already produces both.
- **Against:** it commits the project to defending a *second* number forever, on
  a host that cannot gate absolute timing at all — and the second row's floor
  would have to be set from a measurement taken on this host, which
  [`0023`](./0023-the-gate-that-could-not-gate.md) question 4 has already flagged
  as provisional for exactly this reason. It also invites the reading that
  tf_tree is a 1.8× engine, when the 1.8 is an artifact of how the *comparison*
  is built rather than of the engine.

**Recommendation: A now, B when a quiet host exists — but see question 1, which
narrows how much A is worth.** A is true, cheap and subtractive; B sets a
threshold from numbers this host is not fit to set thresholds with. Doing A does
not foreclose B: the sentence A adds is the sentence B would replace.

What question 1 changes is the *scope* of what A declines to cover. It is not
"most users"; it is **every user who is not building inside this workspace**.
`[profile.embedder]` turns out to be cargo's release defaults on both knobs that
matter, so it is not an approximation of a consumer's build — it is a consumer's
build. That makes A a narrower claim than it first reads, and it is the reason
this record is `draft` rather than a recommendation with a patch attached.

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
2. **Is 1.80 stable, or is it one host's number?** It comes from a single pair of
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
