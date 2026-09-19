#!/usr/bin/env bash
# Every file carrying `unsafe` is one `docs/decisions/0007` rule 1 authorises
# (`just unsafe-budget`; see also `0048`).
#
# Pins a FILE SET under `crates/` and `xtask/`: a new file cannot start carrying
# `unsafe` without a register row, and a row cannot outlive its file. The census
# uses `RUSTFLAGS="--force-warn unsafe_code"` (overrides `forbid`), whose output
# carries no kind, so the register's `kind` column is human bookkeeping. The
# feature matrix is read out of the justfile's one-line `cargo clippy ...
# --all-targets` passes so it cannot drift; `continuation_blind_spots` reports a
# pass the extractor cannot see.
#
# ## What it does NOT prove
#
# * Anything about kinds: a kind-2 file acquiring a kind-4 block stays green.
# * `crates/tf_tree_py` and `crates/tf_tree_tf2_sys` (outside the workspace; rows
#   are `out-of-reach`; covered by `just py-compile` and `just tf2-check`).
# * Paths outside `crates/` and `xtask/`.
# * Feature combinations no justfile clippy line builds.
# * `// SAFETY:` comments or module blocks (0007 rules 3-4). Clippy's
#   `undocumented_unsafe_blocks` (deny) checks placement only, and only on code
#   some clippy line compiles; whether a comment names its invariant is review.
#
# ## The empty-subject question
#
# A collapsed census (wrong feature set, renamed recipe, filter typo) makes
# `census - register` empty and a naive comparison green, so both floors run
# before any comparison and an empty census FAILS. `--self-test` drives the
# comparison over synthetic inputs.
set -euo pipefail

cd "$(dirname "$0")/.."
REG=scripts/unsafe-budget.txt

# Anti-vacuity floors, not budgets: far below the real numbers (see `0048`), they only fail a collapsed census.
MIN_SELECTORS=15
MIN_SITES=100
MIN_REGISTER=20

# The comparison, factored out for `--self-test`. `$1` = census file list; `$2` = register; returns 1 on a violation.
compare() {
    local census_files="$1" register="$2" bad=0
    local allowed out_of_reach required
    allowed=$(mktemp); out_of_reach=$(mktemp); required=$(mktemp)

    grep -v '^[[:space:]]*#' "$register" | grep -v '^[[:space:]]*$' \
        | awk '{ print $2 }' | sort -u > "$allowed"
    grep -v '^[[:space:]]*#' "$register" | grep -v '^[[:space:]]*$' \
        | awk '$3 == "out-of-reach" { print $2 }' | sort -u > "$out_of_reach"
    comm -23 "$allowed" "$out_of_reach" > "$required"

    while read -r f; do
        [ -n "$f" ] || continue
        echo "UNAUTHORISED  $f"
        echo "              carries \`unsafe\` and $register has no row for it."
        echo "              0007 rule 1: name the kind, or do not write the block."
        bad=$((bad + 1))
    done < <(comm -13 "$allowed" <(sort -u "$census_files"))

    while read -r f; do
        [ -n "$f" ] || continue
        echo "STALE         $f"
        echo "              $register authorises it and the census found no \`unsafe\`"
        echo "              in it. Delete the row, or find out why it stopped compiling."
        bad=$((bad + 1))
    done < <(comm -23 "$required" <(sort -u "$census_files"))

    rm -f "$allowed" "$out_of_reach" "$required"
    [ "$bad" -eq 0 ]
}

# The matrix is extracted line by line, so a pass whose `--all-targets` sits past
# a `\` continuation would silently contribute nothing. This reports, by name, a
# clippy pass whose joined form carries `--all-targets` and whose physical line
# does not (and which no exclusion would drop). `$1` = justfile; returns 1 if any.
continuation_blind_spots() {
    awk '
        { sub(/#.*/, "") }
        /cargo clippy/ {
            phys = $0; joined = $0
            while (joined ~ /\\[[:space:]]*$/ && (getline nxt) > 0) {
                sub(/\\[[:space:]]*$/, " ", joined); joined = joined nxt
            }
            if (joined ~ /--all-targets/ && phys !~ /--all-targets/ \
                && joined !~ /--manifest-path/ && joined !~ /--features tf2/ \
                && joined !~ /--fix/) {
                print FILENAME ":" NR ": INVISIBLE TO THE MATRIX: " joined
                n++
            }
        }
        END { exit (n > 0) }
    ' "$1"
}

# Self-test: the ways this check can be wrong, driven over fixtures.
if [ "${1:-}" = "--self-test" ]; then
    d=$(mktemp -d); trap 'rm -rf "$d"' EXIT
    printf '# kind path note\n1 a.rs\n2 b.rs\n3 far.rs out-of-reach\n' > "$d/reg"

    printf 'a.rs\nb.rs\n' > "$d/census"
    out=$(compare "$d/census" "$d/reg") && rc=0 || rc=$?
    [ "$rc" -eq 0 ] && [ -z "$out" ] \
        && echo "self-test 1 ok: a matching census passes, and an out-of-reach row is not required" \
        || { echo "SELF-TEST FAILED: a matching census must pass; rc=$rc out=$out"; exit 1; }

    printf 'a.rs\nb.rs\nc.rs\n' > "$d/census"
    out=$(compare "$d/census" "$d/reg") && rc=0 || rc=$?
    if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q '^UNAUTHORISED  c.rs'; then
        echo "self-test 2 ok: an unregistered file is reported and the exit is non-zero"
    else
        echo "SELF-TEST FAILED: an unregistered file must be reported; rc=$rc"; exit 1
    fi

    printf 'a.rs\n' > "$d/census"
    out=$(compare "$d/census" "$d/reg") && rc=0 || rc=$?
    if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q '^STALE         b.rs'; then
        echo "self-test 3 ok: a register row with no census hit is reported"
    else
        echo "SELF-TEST FAILED: a stale register row must be reported; rc=$rc"; exit 1
    fi

    : > "$d/census"
    out=$(compare "$d/census" "$d/reg") && rc=0 || rc=$?
    if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q '^STALE'; then
        echo "self-test 4 ok: an EMPTY census is reported, not silently passed"
    else
        echo "SELF-TEST FAILED: an empty census must not pass the comparison; rc=$rc"; exit 1
    fi

    printf 'a.rs\nb.rs\nfar.rs\n' > "$d/census"
    out=$(compare "$d/census" "$d/reg") && rc=0 || rc=$?
    [ "$rc" -eq 0 ] \
        && echo "self-test 5 ok: an out-of-reach row that DOES appear is not an error" \
        || { echo "SELF-TEST FAILED: an out-of-reach row must be permitted, not required"; exit 1; }
    printf 'lint:\n    cargo clippy --workspace --all-targets -- -D warnings\n' > "$d/jf_one"
    continuation_blind_spots "$d/jf_one" > /dev/null && rc=0 || rc=$?
    [ "$rc" -eq 0 ] \
        && echo "self-test 6 ok: a one-line clippy pass is visible to the matrix" \
        || { echo "SELF-TEST FAILED: a one-line pass must not be reported; rc=$rc"; exit 1; }

    printf 'lint:\n    cargo clippy --workspace \\\n        --all-targets -- -D warnings\n' \
        > "$d/jf_cont"
    out=$(continuation_blind_spots "$d/jf_cont") && rc=0 || rc=$?
    if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q 'INVISIBLE TO THE MATRIX'; then
        echo "self-test 7 ok: a continuation-written pass is reported by name"
    else
        echo "SELF-TEST FAILED: a \`\\\`-continued --all-targets must be reported; rc=$rc"
        exit 1
    fi

    printf 'x:\n    cargo clippy --manifest-path a/Cargo.toml \\\n        --all-targets\n' \
        > "$d/jf_excl"
    continuation_blind_spots "$d/jf_excl" > /dev/null && rc=0 || rc=$?
    [ "$rc" -eq 0 ] \
        && echo "self-test 8 ok: a continuation the extractor would have excluded anyway is not reported" \
        || { echo "SELF-TEST FAILED: an excluded pass must not be reported; rc=$rc"; exit 1; }

    echo "unsafe-budget: self-test passed (the floors below are what catch an"
    echo "               empty census when the register is also empty)"
    exit 0
fi

# The matrix, read out of the justfile.
mapfile -t SELECTORS < <(
    grep -h 'cargo clippy' justfile \
        | sed 's/#.*//' \
        | grep -- '--all-targets' \
        | grep -v -- '--manifest-path' \
        | grep -v -- '--features tf2' \
        | grep -v -- '--fix' \
        | sed -E 's/^[[:space:]]*cargo clippy[[:space:]]*//;
                  s/[[:space:]]*--[[:space:]]*-D warnings.*$//;
                  s/[[:space:]]*--all-targets//g;
                  s/--bin [A-Za-z0-9_]+//g;
                  s/[[:space:]]+/ /g; s/^ //; s/ $//' \
        | sort -u
)
if ! continuation_blind_spots justfile; then
    echo "unsafe-budget: the line(s) above carry \`--all-targets\` on a continuation, so the"
    echo "               extractor above sees them without it and they contribute NOTHING"
    echo "               to the census — silently. Put the whole invocation on one physical"
    echo "               line, or teach the extractor that shape; do not lower the floor,"
    echo "               which absorbs a drop rather than reporting one."
    exit 1
fi
if [ "${#SELECTORS[@]}" -lt "$MIN_SELECTORS" ]; then
    echo "unsafe-budget: the justfile yielded ${#SELECTORS[@]} clippy selectors, below the"
    echo "               floor of $MIN_SELECTORS. The matrix is read out of the justfile's own"
    echo "               \`cargo clippy … --all-targets\` lines; if those moved, fix this"
    echo "               script rather than lowering the floor — a census over the wrong"
    echo "               commands is the failure this floor exists to catch."
    exit 1
fi

# The census.
raw=$(mktemp); files=$(mktemp)
trap 'rm -f "$raw" "$files"' EXIT

# A failing `cargo check` must fail this script, not quietly contribute zero rows.
for sel in "${SELECTORS[@]}"; do
    out=$(mktemp)
    # shellcheck disable=SC2086  # $sel is a deliberate word-split selector
    if ! RUSTFLAGS="--force-warn unsafe_code" CARGO_INCREMENTAL=0 \
            cargo check -q --all-targets $sel --message-format=short > "$out" 2>&1; then
        echo "unsafe-budget: \`cargo check --all-targets $sel\` failed, so its part of the"
        echo "               census is missing. Fix the build; a census taken over a"
        echo "               subset of the tree cannot answer the question this asks."
        sed -n '1,20p' "$out"
        rm -f "$out"
        exit 1
    fi
    { grep -E '^(crates|xtask)/' "$out" || true; } | { grep -i 'unsafe' || true; } >> "$raw"
    rm -f "$out"
done

sort -u "$raw" -o "$raw"
sites=$(wc -l < "$raw")
if [ "$sites" -lt "$MIN_SITES" ]; then
    echo "unsafe-budget: the census found $sites sites, below the floor of $MIN_SITES."
    echo "               This is not a budget. It is the anti-vacuity floor: an empty or"
    echo "               collapsed census makes every comparison below pass, so a census"
    echo "               that small is a broken instrument and not a clean tree."
    exit 1
fi

register_rows=$(grep -cv '^[[:space:]]*#\|^[[:space:]]*$' "$REG" || true)
if [ "$register_rows" -lt "$MIN_REGISTER" ]; then
    echo "unsafe-budget: $REG has $register_rows rows, below the floor of $MIN_REGISTER."
    exit 1
fi

sed -E 's/:[0-9]+:[0-9]+:.*//' "$raw" | sort -u > "$files"

if compare "$files" "$REG"; then
    echo "unsafe-budget: $(wc -l < "$files") file(s), $sites site(s), over ${#SELECTORS[@]} clippy"
    echo "               selectors read from the justfile — every one has a register row."
else
    echo
    echo "See $REG and docs/decisions/0048-a-kind-is-not-a-crate-name.md."
    exit 1
fi
