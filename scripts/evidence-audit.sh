#!/usr/bin/env bash
# Every runnable artifact is either executed by a recipe or registered as a probe.
#
# See `just evidence-audit` and `docs/benchmarks/EVIDENCE.md` (the register).
# Coverage tests match the shapes that execute a target (`--bench`/`--bin`/
# `--example` selector, `/`-prefixed path component, `CARGO_BIN_EXE_<name>`,
# `sibling_binary("<name>")`), boundary-anchored, never a bare name substring.
set -euo pipefail

cd "$(dirname "$0")/.."
REG=docs/benchmarks/EVIDENCE.md

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# What *executes* a target: recipes, workflows, runner scripts, container
# harnesses, with comment lines stripped (a comment mention is how `abi_cost` hid).
# Written to a file so the tests below can `grep` it without a pipe.
cat justfile .github/workflows/*.yml crates/*/tests/*/run.sh \
    docker/tf2/*.sh scripts/*.sh 2>/dev/null | grep -v '^[[:space:]]*#' > "$work/exec" || true

# Child processes another binary spawns: `CARGO_BIN_EXE_<name>` and
# `sibling_binary("<name>")` (run-time lookup; the only route for `mp_consumer`
# and `load_child`). Checked per target so a target's own source is excluded.
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

# An empty subject set is not a pass: fail if the census or the corpus is empty.
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
    # `grep` against a file, not `printf | grep -q`: SIGPIPE (141) under `pipefail` made results racy.
    # Executed by a recipe, workflow or runner: a cargo selector or the built path;
    # `[^A-Za-z0-9_-]` on the right so `--bench push` is not satisfied by `--bench push_sampler`.
    if grep -Eq -- \
        "(--bench|--bin|--example)[[:space:]]+$tgt([^A-Za-z0-9_-]|\$)|/$tgt([^A-Za-z0-9_-]|\$)" \
        "$work/exec"; then continue; fi
    if grep -Eq -- "CARGO_BIN_EXE_$tgt([^A-Za-z0-9_]|\$)" "$work/spawn"; then continue; fi
    # Spawned by *another* binary. Its own source is excluded, or a target that names itself would excuse itself.
    if grep -rl --include='*.rs' -- "sibling_binary(\"$tgt\")" crates/ xtask/ 2>/dev/null \
       | grep -qv -- "^${src#./}$"; then continue; fi
    # Nothing executes it. It must be registered as a ROW: the name is the first
    # token in backticks of a table row's FIRST cell (bare name, source path, or
    # name plus subcommand), so prose cannot register a target by accident.
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
