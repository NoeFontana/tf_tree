#!/usr/bin/env bash
# Every runnable artifact is either executed by a recipe or registered as a probe.
#
# See `just evidence-audit` for why this exists, and
# `docs/benchmarks/EVIDENCE.md` for the register it enforces. In one line:
# `crates/tf_tree_c/examples/abi_cost.rs` *is* PHASE4 §7 gate criterion 1, it was
# executed by nothing for months, and `docs/PHASE4.md` recorded its stale number
# as a PASS while the example itself printed FAIL.
#
# # A NAME IS NOT A CALL SITE
#
# Until 2026-09-06 every coverage test here was plain substring containment
# against a concatenated corpus, so a target counted as executed whenever its
# name appeared *anywhere* — inside a longer word, inside another recipe's name,
# inside a YAML key. Two criterion benches were live victims and this script was
# green over both: `benches/lookup.rs` was excused by `profile-lookup` and
# `lookup-tf_tree`, and `benches/push.rs` by `push-sampler-cost` and by the bare
# `push:` GitHub Actions trigger key, which will be in the workflows forever.
# Nothing ran either and `EVIDENCE.md` had no row for either.
#
# So the tests below match the *shapes that execute a target* rather than its
# name: a `--bench`/`--bin`/`--example` selector, a `/`-prefixed path component
# (`./target/release/mp_bench`), `CARGO_BIN_EXE_<name>`, or the
# `sibling_binary("<name>")` call one binary uses to find another — each
# boundary-anchored on both sides. That is the `abi_cost` failure arriving
# through the matcher instead of through a comment, and the next one will arrive
# the same way, so a new spelling belongs in the patterns rather than in a
# widened test.
set -euo pipefail

cd "$(dirname "$0")/.."
REG=docs/benchmarks/EVIDENCE.md

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# What *executes* a target: recipes, workflows, the C/C++ runner scripts, the
# container harnesses. **Comment lines are stripped**, because a mention in a
# comment is exactly how `abi_cost` hid — it was named in one `justfile` comment
# and a naive grep would have called it covered.
#
# Written to a file rather than held in a variable so the tests below can be
# `grep`, whose `$` is an end-of-*line* anchor. A pipeline into `grep -q` is what
# this script must not do — see the SIGPIPE note further down.
cat justfile .github/workflows/*.yml crates/*/tests/*/run.sh \
    docker/tf2/*.sh scripts/*.sh 2>/dev/null | grep -v '^[[:space:]]*#' > "$work/exec" || true

# Child processes another binary spawns. Two spellings, and **both are needed**:
#
#   * `CARGO_BIN_EXE_<name>` — how an integration test finds its helper.
#   * `sibling_binary("<name>")` — how one *binary* finds a sibling at run time,
#     e.g. `mp_bench.rs`'s `sibling_binary("mp_consumer")`. There is no
#     compile-time marker for that, and missing it reported `mp_consumer` and
#     `load_child` as unregistered when `just mp-bench` and `just soak` launch
#     them on every run. Those two are the only targets that rest on this arm,
#     and both are spelled exactly that way.
#
# The second arm is checked per target below rather than as one corpus, because
# it must exclude the target's *own* source: a criterion bench naming itself in
# `bench_function("at_many")` would otherwise excuse itself and this check would
# pass trivially for every bench. Anchoring it on `sibling_binary(` rather than
# on a bare quoted string is what stopped `"lookup"` and `"push"` in unrelated
# Rust from doing that anyway.
grep -rh "CARGO_BIN_EXE_[A-Za-z0-9_]*" --include='*.rs' crates/ xtask/ \
    > "$work/spawn" 2>/dev/null || true

cargo metadata --no-deps --format-version 1 | python3 -c '
import json, os, sys
md = json.load(sys.stdin)
for p in md["packages"]:
    for t in p["targets"]:
        for k in t["kind"]:
            if k in ("example", "bin", "bench"):
                rel = os.path.relpath(t["src_path"])
                print(p["name"], k, t["name"], rel, sep="\t")
' > "$work/targets"

# **An empty subject set is not a pass.** Every loop below is over this file; a
# `cargo metadata` that resolved nothing, or a filter that stopped matching,
# would otherwise print the success line having checked nothing. No count is
# written — the shape this repository keeps going stale — only that the census
# is not empty and that the corpus it is compared against is not either.
if [ ! -s "$work/targets" ]; then
    echo "evidence-audit: cargo metadata resolved no bin/example/bench target." >&2
    echo "                That is this script's entire subject set, so it checked nothing." >&2
    exit 1
fi
if [ ! -s "$work/exec" ]; then
    echo "evidence-audit: the execution corpus is empty — justfile, workflows and the" >&2
    echo "                runner scripts produced no lines. Every target would report" >&2
    echo "                unregistered for the wrong reason." >&2
    exit 1
fi

missing=0
while IFS=$'\t' read -r pkg kind tgt src; do
    [ -n "$tgt" ] || continue
    # **`grep` against a file, not `printf | grep -q`.** That spelling made this
    # script report a *different* set of targets on every run: `grep -q` exits at
    # the first match, `printf` then dies of SIGPIPE (141), and under
    # `set -o pipefail` the pipeline reports failure even though the match
    # succeeded. Whether printf had finished writing first is a race. A flaky
    # gate is a gate somebody disables, which is the failure this whole script
    # exists to prevent.
    #
    # Executed by a recipe, a workflow or a runner script: a cargo target
    # selector, or the built path. `[^A-Za-z0-9_-]` on the right, so `--bench
    # push` is not satisfied by `--bench push_sampler`.
    if grep -Eq -- \
        "(--bench|--bin|--example)[[:space:]]+$tgt([^A-Za-z0-9_-]|\$)|/$tgt([^A-Za-z0-9_-]|\$)" \
        "$work/exec"; then continue; fi
    if grep -Eq -- "CARGO_BIN_EXE_$tgt([^A-Za-z0-9_]|\$)" "$work/spawn"; then continue; fi
    # Spawned by *another* binary. **Its own source is excluded**, or a target
    # that names itself would excuse itself. Verified by deleting `just
    # abi-cost` and confirming this script still reports `abi_cost`.
    if grep -rl --include='*.rs' -- "sibling_binary(\"$tgt\")" crates/ xtask/ 2>/dev/null \
       | grep -qv -- "^${src#./}$"; then continue; fi
    # Nothing executes it. It must be registered — as a gate with a recipe, or a
    # probe with what it established. **A ROW, not a mention**: the name must be
    # the first token inside the backticks of a table row's FIRST cell, so a
    # target whose name is also an API name (`at_many`, `read_scaling`) — or
    # which some paragraph in this file happens to discuss — cannot be
    # registered by accident through prose. A bare backtick search was the same
    # name-is-not-a-call-site defect on the register side, and it is live the
    # moment anyone writes a sentence about an unregistered target.
    #
    # The three shapes a first cell takes, all accepted: the bare name
    # (`counter_cost` (bin)), a source path (`tf_tree_bench/benches/push_sampler.rs`),
    # and a name followed by a subcommand (`xtask bench-gate`).
    if ! grep -Eq -- "^\|[[:space:]]*\*{0,2}\`([^\`|]*/)?$tgt(\.rs)?[\` ]" "$REG"; then
        echo "UNREGISTERED  $pkg $kind $tgt"
        echo "              nothing executes it, and $REG has no row for it."
        echo "              Add a recipe (it is a gate) or a row (it is a probe)."
        missing=$((missing + 1))
    fi
done < "$work/targets"

if [ "$missing" -gt 0 ]; then
    echo
    echo "$missing artifact(s) neither executed nor registered. See $REG."
    exit 1
fi
echo "evidence-audit: every runnable artifact is executed by a recipe or registered in $REG"
