# 0025: what build the tf2 ratio gate speaks for

**Status:** ready
**Owner:** @NoeFontana
**Implementation:** the measurement half is already in the tree
(`just tf2-ratio-profiles`, `ratio.rs`'s two estimate constants and the test that
pins both relations). What this record adds is prose: the gated row states which
build it speaks for. **`FLOOR` does not move.**

## Context

`lookup_ratio_vs_tf2` is the headline gated row: tf_tree's depth-3 lookup against
`tf2::BufferCore`'s, a paired median-of-quotients floored at `FLOOR = 2.0`.
`ratio.rs` guards it with `assert!(FLOOR < UNBIASED_ESTIMATE)` (`UNBIASED_ESTIMATE
= 2.25`, every known bias removed), so a biased run cannot clear the floor on
bias alone. **That holds for this workspace's build and not for a consumer's:**

| build | LTO | tf_tree | tf2 | paired ratio |
|---|---|---|---|---|
| workspace `release` | `thin` | 201.55 ns | 504.44 ns | **2.4900×** |
| `[profile.embedder]` | `false` | 244.15 ns | 506.14 ns | **2.0745×** |

`[profile.embedder]` is cargo's release defaults written out. Cargo applies the
top-level package's profile to the whole graph, and `[profile.*]` is honoured only
in a workspace root, so `cargo add tf_tree` **and** `cargo install tf_tree_cli`
both build at `lto = false`, `codegen-units = 16` — `Cargo.toml`'s
`[profile.embedder]` exactly. The workspace's 2.49× is reachable only inside this
repository.

At that build the paired ratio still clears 2.0, but the unbiased estimate does
not: `UNBIASED_ESTIMATE_DEFAULT_RELEASE ≈ 1.80×`, so the gate would pass *on
binding bias*, which the assertion exists to prevent. The cross-profile read is
legitimate: tf2 moves **+0.34%** across the profile change while tf_tree moves
**+21.1%** (an `extern "C"` call into a C++ shim cannot be inlined by any Rust LTO
setting).

## The three repeats

Same container, `taskset -c 2`, 9 rounds × 10240 lookups per arm:

| run | workspace `release` | verdict | `[profile.embedder]` | verdict |
|---|---|---|---|---|
| 1 | 2.4869x, band 2.464-2.777 | ABOVE | 2.0884x, band 2.080-2.137 | ABOVE |
| 2 | 2.4439x, band 2.430-2.462 | ABOVE | 2.0833x, band **1.975**-2.563 | **UNRESOLVED** |
| 3 | 2.5073x, band 2.446-2.856 | ABOVE | 2.0474x, band **1.923**-2.340 | **UNRESOLVED** |

**The medians are stable** (consumer 2.047-2.088, ~2% spread), so the number is
real. **The consumer verdict is not resolvable on this host**: `verdict` compares
the **band** against `FLOOR`, and the band straddles 2.0 in two runs of three. A
row whose band crosses its own floor is not one to hang a threshold on.

## Decision

**`FLOOR` stays at 2.0, the gate speaks for the workspace build, and the row says
so.** No second gated row, no change to any constant. The row's prose becomes, in
substance: *this gate speaks for `lto = "thin"`, which this workspace ships. A
consumer at cargo's release defaults measures about 2.07x, which this host cannot
resolve against 2.0 (two runs in three UNRESOLVED); with the binding bias removed
it is ~1.80x, under the floor. This gate does not speak for that build.*

### Why not a second gated row at `[profile.embedder]`

It needs a floor for the consumer row, and the only basis is this host, which
returns UNRESOLVED for that row in most runs. A threshold set from a band that
contains it is the failure `0023` question 4 flags for R1's provisional 1.10. A
floor low enough to pass reliably (under 1.80) would be chosen to be passed, and
**a gate that always passes is worse than none, because it reads as evidence.**

The cost is stated: the build declined is every build outside this repository.
**B is not foreclosed**; its precondition is a host that returns a resolved verdict
for the consumer row across repeated runs, after which `just tf2-ratio-profiles`
would derive the threshold.

## What is NOT open

- **`FLOOR` does not move down**, and `assert!(FLOOR < UNBIASED_ESTIMATE)` stays, as
  does `the_floor_is_bounded_at_one_profile_and_not_the_other` (which also asserts
  `FLOOR > UNBIASED_ESTIMATE_DEFAULT_RELEASE`, so the deficiency cannot be quietly
  resolved by editing a constant).
- **Which number is quoted in prose**: `UNBIASED_ESTIMATE_DEFAULT_RELEASE` is `pub`
  and `UNBIASED_ESTIMATE` is private on purpose; the weaker figure is the one a
  reader can reach.
