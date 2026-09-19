# 0025: what build the tf2 ratio gate speaks for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** the measurement half is already in the tree
(`just tf2-ratio-profiles`, `ratio.rs`'s two estimate constants and the test that
pins both relations). What this record adds is prose: the gated row states which
build it speaks for. **`FLOOR` does not move.**

## Context

`lookup_ratio_vs_tf2` is the headline gated row: a paired median-of-quotients
floored at `FLOOR = 2.0`. `ratio.rs` guards it with `assert!(FLOOR <
UNBIASED_ESTIMATE)` (2.25, every known bias removed). **That holds for this
workspace's build and not for a consumer's:**

| build | LTO | paired ratio |
|---|---|---|
| workspace `release` | `thin` | **2.4900×** |
| `[profile.embedder]` | `false` | **2.0745×** |

`[profile.embedder]` is cargo's release defaults, where every consumer builds. Its
unbiased estimate, `UNBIASED_ESTIMATE_DEFAULT_RELEASE` ≈ 1.80×, is under the floor,
and its band straddles 2.0 on this host.

## Decision

**`FLOOR` stays at 2.0, the gate speaks for the workspace build, and the row says
so:** this gate speaks for `lto = "thin"`; a consumer at cargo's release defaults
measures about 2.07x, unresolvable against 2.0 on this host and ~1.80x with the
binding bias removed, and this gate does not speak for that build.

No second gated row at `[profile.embedder]`: its floor could only be set from a
band that contains it, or low enough to always pass, and **a gate that always
passes is worse than none.** Its precondition is a host that resolves the consumer
row across repeated runs.

## What is NOT open

- **`FLOOR` does not move down**, and `assert!(FLOOR < UNBIASED_ESTIMATE)` stays,
  as does `the_floor_is_bounded_at_one_profile_and_not_the_other` (which also
  asserts `FLOOR > UNBIASED_ESTIMATE_DEFAULT_RELEASE`).
- **Which number is quoted in prose**: `UNBIASED_ESTIMATE_DEFAULT_RELEASE` is `pub`
  and `UNBIASED_ESTIMATE` is private on purpose.
