#!/usr/bin/env bash
# Every runnable artifact is either executed by a recipe or registered as a probe.
#
# See `just evidence-audit` for why this exists, and
# `docs/benchmarks/EVIDENCE.md` for the register it enforces. In one line:
# `crates/tf_tree_c/examples/abi_cost.rs` *is* PHASE4 §7 gate criterion 1, it was
# executed by nothing for months, and `docs/PHASE4.md` recorded its stale number
# as a PASS while the example itself printed FAIL.
set -euo pipefail

cd "$(dirname "$0")/.."
REG=docs/benchmarks/EVIDENCE.md

# What *executes* a target: recipes, workflows, the C/C++ runner scripts, the
# container harnesses. **Comment lines are stripped**, because a mention in a
# comment is exactly how `abi_cost` hid — it was named in one `justfile` comment
# and a naive grep would have called it covered.
exec_corpus=$(cat justfile .github/workflows/*.yml crates/*/tests/*/run.sh \
                  docker/tf2/*.sh scripts/*.sh 2>/dev/null | grep -v '^[[:space:]]*#' || true)

# Child processes another binary spawns. Two spellings, and **both are needed**:
#
#   * `CARGO_BIN_EXE_<name>` — how an integration test finds its helper.
#   * a quoted string in Rust source — how one *binary* finds a sibling at run
#     time, e.g. `mp_bench.rs`'s `sibling_binary("mp_consumer")`. There is no
#     compile-time marker for that, and missing it reported `mp_consumer` and
#     `load_child` as unregistered when `just mp-bench` and `just soak` launch
#     them on every run.
#
# The quoted-string arm is checked per target below rather than as one corpus,
# because it must exclude the target's *own* source: a criterion bench naming
# itself in `bench_function("at_many")` would otherwise excuse itself and this
# check would pass trivially for every bench.
spawn_corpus=$(grep -rh "CARGO_BIN_EXE_[A-Za-z0-9_]*" --include='*.rs' crates/ xtask/ 2>/dev/null || true)

targets=$(cargo metadata --no-deps --format-version 1 | python3 -c '
import json, sys
md = json.load(sys.stdin)
for p in md["packages"]:
    for t in p["targets"]:
        for k in t["kind"]:
            if k in ("example", "bin", "bench"):
                import os
                rel = os.path.relpath(t["src_path"])
                print(p["name"], k, t["name"], rel, sep="\t")
')

missing=0
while IFS=$'\t' read -r pkg kind tgt src; do
    [ -n "$tgt" ] || continue
    # **Bash substring tests, not `printf | grep -q`.** That spelling made this
    # script report a *different* set of targets on every run: `grep -q` exits at
    # the first match, `printf` then dies of SIGPIPE (141), and under
    # `set -o pipefail` the pipeline reports failure even though the match
    # succeeded. Whether printf had finished writing first is a race. A flaky
    # gate is a gate somebody disables, which is the failure this whole script
    # exists to prevent.
    [[ "$exec_corpus"   == *"$tgt"* ]]                  && continue
    [[ "$spawn_corpus"  == *"CARGO_BIN_EXE_$tgt"* ]]    && continue
    # Named as a quoted string by *another* Rust file — the `sibling_binary`
    # case. **Its own source is excluded**, or a criterion bench that names
    # itself in `bench_function("at_many")` would excuse itself and the check
    # would pass trivially for every bench. Verified by deleting `just
    # abi-cost` and confirming this script still reports `abi_cost`.
    if grep -rl --include='*.rs' -- "\"$tgt\"" crates/ xtask/ 2>/dev/null \
       | grep -qv -- "^${src#./}$"; then continue; fi
    # Nothing executes it. It must be registered — as a gate with a recipe, or a
    # probe with what it established. Matched inside backticks so a target whose
    # name is also an API name (`at_many`, `read_scaling`) cannot be registered
    # by accident through unrelated prose.
    if ! grep -q "\`$tgt\`" "$REG"; then
        echo "UNREGISTERED  $pkg $kind $tgt"
        echo "              nothing executes it, and $REG has no row for it."
        echo "              Add a recipe (it is a gate) or a row (it is a probe)."
        missing=$((missing + 1))
    fi
done <<< "$targets"

if [ "$missing" -gt 0 ]; then
    echo
    echo "$missing artifact(s) neither executed nor registered. See $REG."
    exit 1
fi
echo "evidence-audit: every runnable artifact is executed by a recipe or registered in $REG"
