#!/usr/bin/env bash
# No tracked file is build output (`just no-build-output`).
#
# Checks what files *are*, not where they sit, because `CARGO_TARGET_DIR` can
# point anywhere and `.gitignore` path rules miss each new spelling. Signatures
# (each is a file only a build tool writes):
#   * `CACHEDIR.TAG`  — cache-directory marker; cargo writes one per target dir.
#   * `.fingerprint/` — cargo's staleness database.
#   * `.rustc_info.json`, `.cargo-lock`, `.cargo-build-lock`
#
# Deliberately NOT matched: `*.rlib`, `*.o`, `*.d`, `deps/`, `incremental/` —
# each has a plausible legitimate tracked instance and would make the gate flap.
set -euo pipefail

cd "$(dirname "$0")/.."

# `git ls-files`: the defect is *tracked* build output; untracked is `.gitignore`'s job.
hits=$(git ls-files | grep -E '(^|/)(CACHEDIR\.TAG|\.rustc_info\.json|\.cargo-lock|\.cargo-build-lock)$|(^|/)\.fingerprint/' || true)

if [ -z "$hits" ]; then
    echo "no-build-output: OK — no tracked file carries a build-output signature"
    exit 0
fi

count=$(printf '%s\n' "$hits" | wc -l | tr -d ' ')

# Report the directories, not every path.
echo "no-build-output: FAIL — $count tracked file(s) are build output." >&2
echo >&2
echo "Roots:" >&2
printf '%s\n' "$hits" | sed 's#/.*##' | sort -u | sed 's/^/  /' >&2
echo >&2
echo "This is build output, committed. To fix:" >&2
echo >&2
echo "    git rm -r --cached <root>        # untrack, keep on disk" >&2
echo "    # then add the directory to .gitignore" >&2
echo >&2
echo "If you set CARGO_TARGET_DIR to run parallel builds, point it OUTSIDE the" >&2
echo "repository (\$TMPDIR, ~/.cache) rather than at a sibling of target/. That" >&2
echo "is what put 358 MiB into this history; see the Rust section of .gitignore." >&2
exit 1
