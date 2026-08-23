#!/usr/bin/env bash
# No tracked file carries an unresolved merge-conflict marker.
#
# See `just no-conflict-markers`. This is the fifth gate of the family that
# starts with `just msrv`'s third arm, `scripts/evidence-audit.sh`,
# `scripts/artifact-versions.py` and `scripts/no-build-output.sh`: a property of
# the *repository* rather than of the code, checkable exactly, and invisible to
# every test.
#
# **What it cost to not have it.** `docs/decisions/README.md` reached `main` at
# 48a5620 with a three-way conflict written into the middle of its status table
# — `<<<<<<< Updated upstream`, `||||||| Stash base`, `=======`,
# `>>>>>>> Stashed changes` — and three copies of the `0033`/`0034`/`0035` rows,
# two of them stale. The cause was `git rebase` on a dirty worktree: the
# autostash popped with a conflict, the rebase itself reported success, and
# `git status` was clean afterwards because the markers were *in the file* and
# the file was staged in the same breath.
#
# **It passed everything.** All eighteen CI checks were green, and so was
# `just lint` locally. `artifact-versions.py` reads that exact table on every
# run — and counts *cells per row against the header*, which conflict markers
# are not, because they are not table rows at all. The duplicated rows it did
# see were well-formed. Nothing else in the workspace reads a Markdown table for
# anything but its cell count.
#
# **Why a signature and not "review your diffs".** The failure mode is precisely
# that the diff looked right: the rebase said "Successfully rebased", the record
# file it was about was correct, and the corruption was in a neighbouring file
# nobody thought to re-read. A gate does not get tired at the end of a stack of
# five pull requests.
#
# **Why these three markers and not `=======`.** Measured against the whole
# tracked corpus first, per `artifact-versions.py`'s standard — a gate that flaps
# is a gate people learn to pass by editing the gate. `<<<<<<< `, `>>>>>>> ` and
# `||||||| ` at the start of a line, seven characters and a space, matched
# exactly the one corrupted file and nothing else in the repository.
#
# `=======` on a line by itself is deliberately NOT matched. It is half of every
# conflict, but it is also a Markdown setext heading underline, and this
# repository is more documentation than code. Every conflict git writes carries
# the `<<<<<<<`/`>>>>>>>` pair, so dropping the ambiguous third costs no coverage
# and removes the only plausible false positive.
set -euo pipefail

cd "$(dirname "$0")/.."

# `git ls-files` and not a filesystem walk: an unresolved marker in an untracked
# scratch file is nobody's business. What must never happen is committing one.
# `-I` skips binaries, which cannot carry a marker and can carry the bytes.
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
