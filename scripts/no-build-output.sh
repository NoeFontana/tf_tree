#!/usr/bin/env bash
# No tracked file is build output.
#
# See `just no-build-output`. This is the fourth gate of the family that starts
# with `just msrv`'s third arm, `scripts/evidence-audit.sh` and
# `scripts/artifact-versions.py`: a property of the *repository* rather than of
# the code, checkable exactly, and invisible to every test.
#
# **What it cost to not have it.** `CARGO_TARGET_DIR=target-p` is how you run two
# cargo invocations without them fighting over one lock, and `target-p` is a
# sibling of `target/`, not a child — so `.gitignore`'s anchored `/target/`
# matched none of it. Four such directories were committed and merged across
# three pull requests (#237, #242, #243): 1386 files, 358 MiB, taking a fresh
# clone from 5 MiB to 112 MiB and the v0.0.4 GitHub source tarball from 9 MiB to
# 119 MiB. The published crates.io and PyPI artifacts were unaffected — both
# package from a crate root, and the junk sat above every one of them — so no
# test, no lint and no release gate could have seen it. `git status` was clean,
# because the files were tracked.
#
# **Why a signature and not a path list.** `.gitignore` has now been patched
# three times for this same trap, once per spelling: `/crates/*/target/`, `/log/`,
# and now `/target*/`. A fourth spelling (`build-p/`, `/tmp/cargo-out`, a
# `CARGO_TARGET_DIR` pointed anywhere at all) would slip past all three. So this
# checks what the files *are*, not where they sit.
#
# **Why these four signatures and no others.** The rule was measured against the
# whole tracked corpus before it was written, per `artifact-versions.py`'s
# standard: a gate that flaps is a gate people learn to pass by editing the gate.
# Against the parent commit of this one the four patterns matched 741 tracked
# files, every one of them inside the four junk directories and none anywhere
# else in the corpus. Against this commit they match zero. That is the entire basis for calling
# them exact — narrow enough to have no false positive, and each one is a file a
# build tool writes and a human never does:
#
#   * `CACHEDIR.TAG`  — the cache-directory standard's own marker. Its literal
#                       meaning is "this directory is regenerable, do not back it
#                       up", so a *tracked* one is a contradiction in terms.
#                       Cargo writes one into every target directory.
#   * `.fingerprint/` — cargo's staleness database.
#   * `.rustc_info.json`, `.cargo-lock`, `.cargo-build-lock`
#                     — cargo's compiler probe cache and its build locks.
#
# Deliberately NOT matched: `*.rlib`, `*.o`, `*.d`, `deps/`, `incremental/`.
# Each would have caught this too, and each has a plausible legitimate tracked
# instance (a vendored archive, a fixture object file, a directory named `deps`),
# which is exactly the flap this file refuses to introduce.
set -euo pipefail

cd "$(dirname "$0")/.."

# `git ls-files` and not a filesystem walk: the defect is *tracked* build output.
# Untracked build output in the working tree is normal and is `.gitignore`'s job.
hits=$(git ls-files | grep -E '(^|/)(CACHEDIR\.TAG|\.rustc_info\.json|\.cargo-lock|\.cargo-build-lock)$|(^|/)\.fingerprint/' || true)

if [ -z "$hits" ]; then
    echo "no-build-output: OK — no tracked file carries a build-output signature"
    exit 0
fi

count=$(printf '%s\n' "$hits" | wc -l | tr -d ' ')

# Report the *directories*, not 1386 paths. What a reader needs is which tree to
# untrack; the individual fingerprint files are noise.
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
