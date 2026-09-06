#!/usr/bin/env bash
# Every file carrying `unsafe` is one `docs/decisions/0007` rule 1 authorises.
#
# See `just unsafe-budget`, `docs/decisions/0007-the-unsafe-budget-and-the-c-abi.md`
# and `docs/decisions/0048-a-kind-is-not-a-crate-name.md`.
#
# **What it pins is a FILE SET under `crates/` and `xtask/`, and that is the
# whole of its claim.** The census
# is taken with `RUSTFLAGS="--force-warn unsafe_code"`, and that lint's output is
# `file:line: warning: usage of an `unsafe` block` — it carries **no kind**. So
# the `kind` column in the register beside this script is bookkeeping a human
# maintains, and 0007 rule 1's *criterion* stays a review rule. What this check
# holds is that a **new file** cannot start carrying `unsafe` without somebody
# writing down which kind it is, and that a register row cannot outlive the file
# it names.
#
# **Why the compiler and not a grep.** Two greps over `crates/`, wrong in
# opposite directions: a plain `grep -rn unsafe` over-counts, and dozens of its
# hits are `unsafe_code` / `unsafe_op_in_unsafe_fn` **attributes** — the rule's
# own enforcement mechanism, counted as violations of it. The word-boundary form
# `grep -rn -E '\bunsafe\b'` drops those (there is no word boundary before `_`)
# and still over-counts by more than a hundred lines, because this repository's
# prose *about* `unsafe` is unusually dense and every `// SAFETY:` paragraph that
# uses the word counts. `docs/decisions/0048` tabulates all of it with the
# commands. `--force-warn` overrides `#![forbid(unsafe_code)]` rather than being
# suppressed by it, so a crate root that forbids is still censused.
#
# **Why the matrix comes from the justfile.** `unsafe` behind a feature no
# command builds is invisible to any census, and this repository has several:
# `footprint` builds by default, `abi_attached` needs `abi-probe`, `fork_child`
# needs `shm` and its fourth mode `bridge`. Re-spelling that list here would let
# it drift from the clippy passes it is meant to mirror, so the selectors are
# **read out of the justfile's own `cargo clippy … --all-targets` lines**. A new
# feature-named pass therefore enters this census the day it is added.
#
# ## What it does NOT prove
#
# * **Nothing about kinds.** See above. A file authorised as kind 2 that acquires
#   a kind-4 block is green here.
# * **Nothing about `crates/tf_tree_py` or `crates/tf_tree_tf2_sys`.** Both are
#   excluded from the cargo workspace, so no `-p` selector reaches them. Their
#   register rows are marked `out-of-reach` and are neither required nor
#   forbidden in the census. What covers them is `just py-compile` and the
#   container-only `just tf2-check`; the counts in `0048` were taken by running
#   this same census against their own manifests, by hand.
# * **Nothing about a path outside `crates/` and `xtask/`.** The census lines are
#   filtered to those two roots, so a workspace member added anywhere else — or a
#   `build.rs` whose diagnostics carry a different prefix — is invisible until the
#   filter is widened with it. `xtask` was invisible for exactly this reason until
#   2026-09-05, and it is a workspace member the `--workspace` selector compiles.
# * **Nothing about a feature combination no justfile clippy line builds.** That
#   is the same hole `just shm-check`'s own comments already name, and mirroring
#   the justfile is how it stays exactly that hole and no larger.
# * **Nothing about `// SAFETY:` comments or module blocks** — 0007 rules 3 and
#   4. Those are review rules and this script does not read them.
#
# ## The empty-subject question
#
# A violation inside the covered path set *adds* a row to the census, so one
# failure mode is the census collapsing — a wrong feature set, a renamed recipe,
# a filter typo, a cargo invocation that printed nothing. `census − register` is
# then empty and a naive comparison is GREEN. So both floors below run **before**
# any comparison, and an empty census is a FAILURE.
# `bash scripts/unsafe-budget.sh --self-test` drives the comparison over
# synthetic inputs and asserts each verdict. It is one failure mode and not the
# only one: the disclosed holes above are the others, and they are holes in the
# census's *reach* rather than in its comparison.
set -euo pipefail

cd "$(dirname "$0")/.."
REG=scripts/unsafe-budget.txt

# Anti-vacuity floors, not budgets. They exist to fail a *collapsed* census, so
# they sit far below the real numbers and never need updating when a file gains
# or loses a block. The real counts live in `0048` with the command that
# produced them.
MIN_SELECTORS=15
MIN_SITES=100
MIN_REGISTER=20

# ---------------------------------------------------------------------------
# The comparison, factored out so `--self-test` can drive it over fixtures.
# `$1` = file holding the census file list; `$2` = register file.
# Prints violations, returns 1 if any.
# ---------------------------------------------------------------------------
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

# ---------------------------------------------------------------------------
# Self-test: the three ways this check can be wrong, driven over fixtures.
# ---------------------------------------------------------------------------
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
    echo "unsafe-budget: self-test passed (the floors below are what catch an"
    echo "               empty census when the register is also empty)"
    exit 0
fi

# ---------------------------------------------------------------------------
# The matrix, read out of the justfile.
# ---------------------------------------------------------------------------
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
if [ "${#SELECTORS[@]}" -lt "$MIN_SELECTORS" ]; then
    echo "unsafe-budget: the justfile yielded ${#SELECTORS[@]} clippy selectors, below the"
    echo "               floor of $MIN_SELECTORS. The matrix is read out of the justfile's own"
    echo "               \`cargo clippy … --all-targets\` lines; if those moved, fix this"
    echo "               script rather than lowering the floor — a census over the wrong"
    echo "               commands is the failure this floor exists to catch."
    exit 1
fi

# ---------------------------------------------------------------------------
# The census.
# ---------------------------------------------------------------------------
raw=$(mktemp); files=$(mktemp)
trap 'rm -f "$raw" "$files"' EXIT

# **A failing `cargo check` must fail this script, not quietly contribute zero
# rows.** A selector that does not compile loses its whole contribution to the
# census, and `census − register` then reports STALE rows rather than the build
# error that caused them — which reads as a tidy-up job rather than as a broken
# tree. The floors below catch a *total* collapse; this catches one selector's.
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
