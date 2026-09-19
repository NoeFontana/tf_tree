#!/usr/bin/env bash
# Run a gate recipe and read its exit code against a declared policy.
#
# `crates/tf_tree_bench/src/gate.rs` fixes the exit codes: 0 PASS, 1 FAIL,
# 2 REFUSED (not evaluated). This script is the only place that interprets them.
# The caller states which reading is acceptable:
#
#   must-pass    0 -> pass.  1 -> fail.  2 -> FAIL.
#   may-refuse   0 -> pass.  1 -> fail.  2 -> pass, and emit a ::warning::
#                (a refusal is a host fact, kept visible rather than silently green).
#   must-refuse  2 -> pass.  0 -> FAIL, naming the doc row to update.  1 -> fail.
#                (stops a permanent refusal going vacuous.)
#
# Unknown policies and recipes are refused, never defaulted.
set -uo pipefail

usage() {
    echo "usage: $0 <just-recipe> <must-pass|may-refuse|must-refuse>" >&2
    exit 64
}

# `--self-test` drives all nine (policy x code) cells against a stub; a mis-read cell would only colour the job wrong.
if [ "${1:-}" = "--self-test" ]; then
    stub=$(mktemp -d)
    trap 'rm -rf "$stub"' EXIT
    # The stub models `just`: `--show` (recipe-existence probe) succeeds; running returns the code under test.
    printf '#!/usr/bin/env bash\ncase "${1:-}" in --show) exit 0 ;; esac\nexit ${STUB_CODE:-0}\n' >"$stub/just"
    chmod +x "$stub/just"
    fails=0
    # policy code expected
    while read -r pol code want; do
        [ -n "$pol" ] || continue
        STUB_CODE="$code" PATH="$stub:$PATH" "$0" stub-recipe "$pol" >/dev/null 2>&1
        got=$?
        if [ "$got" -ne "$want" ]; then
            echo "gate-run self-test: $pol on exit $code gave $got, want $want" >&2
            fails=$((fails + 1))
        fi
    done <<'CELLS'
must-pass 0 0
must-pass 1 1
must-pass 2 1
may-refuse 0 0
may-refuse 1 1
may-refuse 2 0
must-refuse 0 1
must-refuse 1 1
must-refuse 2 0
CELLS
    if [ "$fails" -ne 0 ]; then
        echo "gate-run self-test: $fails of 9 cells wrong" >&2
        exit 1
    fi
    echo "gate-run self-test: all 9 (policy x exit code) cells read correctly"
    exit 0
fi

[ "$#" -eq 2 ] || usage
recipe="$1"
policy="$2"

case "$policy" in
must-pass | may-refuse | must-refuse) ;;
*)
    echo "gate-run: unknown policy '$policy'" >&2
    usage
    ;;
esac

# A recipe that does not exist must not read as a refusal.
if ! just --show "$recipe" >/dev/null 2>&1; then
    echo "gate-run: '$recipe' is not a just recipe" >&2
    exit 64
fi

echo "gate-run: just $recipe (policy: $policy)"
just "$recipe"
code=$?

case "$policy:$code" in
must-pass:0)
    echo "gate-run: PASS — '$recipe' was evaluated and holds."
    exit 0
    ;;
must-pass:2)
    echo "gate-run: FAIL — '$recipe' REFUSED to evaluate under must-pass." >&2
    echo "  This host was expected to be able to hold this gate. A refusal here" >&2
    echo "  is a finding about the runner, not a free pass. Re-read the binary's" >&2
    echo "  printed reason above." >&2
    exit 1
    ;;
may-refuse:0)
    echo "gate-run: PASS — '$recipe' was evaluated and holds."
    echo "  (policy is may-refuse; if this host is now reliably able to evaluate"
    echo "   it, must-pass is the tighter policy.)"
    exit 0
    ;;
may-refuse:2)
    echo "::warning::'$recipe' REFUSED — not evaluated on this host."
    echo "gate-run: pass by policy; the reason the binary printed is the record."
    exit 0
    ;;
must-refuse:2)
    echo "gate-run: PASS — '$recipe' REFUSED, as the policy requires."
    exit 0
    ;;
must-refuse:0)
    echo "gate-run: FAIL — '$recipe' was EVALUATED and passed under must-refuse." >&2
    echo "  This is not a bad thing; it is an out-of-date document. Something" >&2
    echo "  that could not be measured now can be. Update the criterion's row" >&2
    echo "  and move the policy to must-pass in the same change." >&2
    exit 1
    ;;
*:1)
    echo "gate-run: FAIL — '$recipe' was evaluated and the criterion does not hold." >&2
    exit 1
    ;;
*)
    echo "gate-run: FAIL — '$recipe' exited $code, which is not part of the" >&2
    echo "  PASS/FAIL/REFUSED contract in crates/tf_tree_bench/src/gate.rs." >&2
    echo "  A gate binary must leave through Outcome::report_and_exit." >&2
    exit 1
    ;;
esac
