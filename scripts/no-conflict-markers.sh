#!/usr/bin/env bash
# No tracked file carries an unresolved merge-conflict marker (`just no-conflict-markers`).
#
# Matches `<<<<<<< `, `>>>>>>> ` and `||||||| ` at line start. `=======` alone is
# deliberately not matched: it is also a Markdown setext underline, and every
# git conflict carries the `<<<<<<<`/`>>>>>>>` pair anyway.
set -euo pipefail

cd "$(dirname "$0")/.."

# `git ls-files`, not a walk: untracked scratch files do not matter. `-I` skips binaries.
hits=$(git ls-files -z | xargs -0 grep -InE '^(<{7}|>{7}|\|{7}) ' 2>/dev/null || true)

if [ -z "$hits" ]; then
    echo "no-conflict-markers: OK — no tracked file carries an unresolved conflict marker"
    exit 0
fi

count=$(printf '%s\n' "$hits" | wc -l | tr -d ' ')

echo "no-conflict-markers: FAIL — $count line(s) look like an unresolved merge conflict." >&2
echo >&2
printf '%s\n' "$hits" | sed 's/^/  /' >&2
echo >&2
echo "Resolve the conflict and delete every marker line, then re-run." >&2
echo >&2
echo "If this came from a rebase, note that a rebase on a DIRTY worktree" >&2
echo "autostashes and pops, and the pop can conflict *after* the rebase has" >&2
echo "already reported success. Commit or stash first, and read the diff of" >&2
echo "every file it touched — not just the one you were editing." >&2
exit 1
