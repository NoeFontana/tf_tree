#!/usr/bin/env bash
# **`docs/PHASE5.md` §5.1's NORMATIVE CI test, which did not exist.**
#
# §5.1: *"run the full test suite under `strace` (or a seccomp filter) and
# assert that `socket(2)` is called only with `AF_UNIX`"*, because the claim it
# backs — "the `tf_tree` **library** opens no network sockets. Ever." — is one a
# robotics team makes a procurement decision on, and §5.1 says in as many words
# that a promise in a README is worth less than an assertion in CI. Before this
# script, every hit of `git grep -nE 'AF_UNIX|AF_INET|seccomp|strace' main` was
# prose or a comment — **not one was an assertion**, and that is the half the
# argument rests on. **A hit count used to be published here, in
# `docs/PHASE5.md` §5.1's amendment and in the justfile.** It was published
# wrong, corrected once, and then printed beside a basic-regex spelling of the
# command in which `|` is a literal, so the instrument and the measurement
# disagreed. It is deleted rather than corrected a third time: run the command
# above if you want the size of it.
#
# ## PROVES
#
#   * Every `socket(2)` issued by the **library**'s test binaries — the five
#     published crates, `tf_tree`, `tf_tree_core`, `tf_tree_math`,
#     `tf_tree_arena`, `tf_tree_ipc`, at the feature set below — names
#     `AF_UNIX`, over the whole process tree (`strace -f`, so a spawned
#     rendezvous child is traced too).
#   * That at least one `AF_UNIX` socket was in fact observed. "No non-`AF_UNIX`
#     socket" is also what a run that created no socket at all reports, and the
#     Phase 2 rendezvous is the one socket the library is *supposed* to have.
#   * That `strace` on this host can see a `socket(2)` and that the scanner
#     below flags a non-`AF_UNIX` one, checked live on every run against a real
#     `AF_INET` socket (see `self_check`). A check whose instrument has stopped
#     working reports exactly what a clean run does.
#   * That the **known exception is still visible to the scanner**:
#     `tf_tree_cli`'s `web` test target really does open `AF_INET` sockets, and
#     this script asserts that it finds them there. That is the difference
#     between "the library opens no network socket" and "this check is scoped so
#     narrowly it could not see one" — the second is also green. The exception is
#     therefore named and *measured*, not assumed, and if `tf_tree top --web`
#     ever stopped binding a listener this run would say so rather than quietly
#     losing its only positive control over the real code.
#
# ## DOES NOT PROVE
#
#   * Anything about `tf_tree_cli` **except that it does open one**. `tf_tree
#     top --web` binds an `AF_INET` listener by construction and §5.1 scopes the
#     headline to the library for that reason;
#     `crates/tf_tree_cli/src/web.rs`'s design note is where that exception is
#     written down. The scope is a package list here rather than an exception
#     list inside the scanner — an assertion that has learned to ignore an
#     `AF_INET` socket has stopped being this assertion — and the CLI is traced
#     separately, as the positive control, rather than not traced at all.
#     Nothing here says the CLI's listener is *correctly* bound to loopback;
#     `crates/tf_tree_cli/tests/web.rs` is what asserts that.
#   * Anything about a socket the library never creates but *inherits* — an fd
#     passed in by a parent, which is exactly what `tf_tree_ipc`'s fd passing
#     does with the rendezvous socket. `socket(2)` is the syscall §5.1 names;
#     `connect(2)` and `sendto(2)` on a received fd are not traced.
#   * Anything about code paths no test takes. This is a dynamic check over the
#     tests that exist, not a static one over the crate: an `AF_INET` socket
#     behind a branch nothing exercises is invisible to it, and no amount of
#     `strace` changes that.
#   * Anything about a non-Linux target, or about a build with different
#     features. `--features tf_tree/shm,tf_tree_arena/shm` is used deliberately:
#     without `shm` the Phase 2 rendezvous is compiled out, and the socket the
#     claim is *about* is then never opened. **That does not reduce the socket
#     count to zero** — with `shm` dropped the total lands at 23 rather than 0,
#     because `tf_tree_ipc`'s own tests open theirs regardless. That is why the
#     anti-vacuity floor below names the `tf_tree::rendezvous` binary rather
#     than counting sockets. An earlier revision of this header claimed a bare
#     count would catch it, and the run that was supposed to demonstrate that
#     passed. (The green run's own total is **not** quoted here: it is
#     run-dependent on one host, and the script prints it every time.)
#   * Anything about a package that is neither in `PACKAGES` below nor the
#     control. The traced set is exactly `PACKAGES` — the five published crates,
#     what a downstream links — plus `tf_tree_cli` as the control; **everything
#     else in this repository is traced by nothing.** That follows from the
#     `PACKAGES` list by construction, and it is worth saying because "asserted
#     in CI" over a five-crate subset reads like "asserted over the repository"
#     to somebody who did not open this file. (This bullet used to enumerate the
#     untraced packages. The list was published incomplete — it missed
#     `crates/tf_tree_tf2_sys`, the ROS 2 bridge, and `xtask` — so the rule
#     replaces it rather than a longer list: `cargo metadata --no-deps` and
#     `ls crates/` are what generate it.)
#
# ## RED TESTS
#
# Every refusal below was seeded and observed to fire before it was trusted, on
# 2026-09-04. Recorded here rather than in a commit message, because a check
# nobody has seen fail is a check nobody has tested:
#
#   * A real `TcpListener::bind("127.0.0.1:0")` added as a library test — the
#     one that matters — names the offending binary and the `socket(AF_INET,…)`
#     line, EXIT=1.
#   * `strace` absent from `PATH`; a stub `strace` that runs the program and
#     truncates the log; a panicking library test; a `FEATURES` value that
#     lists no binary; `shm` dropped from `FEATURES` (24 binaries, 23 sockets —
#     **not** zero, which is why the floor names a binary); a stub `strace` that
#     empties only the rendezvous log — one refusal each, EXIT=1. `FEATURES` is
#     a constant below and deliberately not read from the environment, so the
#     two feature-set arms are seeded by editing that line, not by exporting it.
#   * **Two arms are live and were seeded by a reviewer rather than by the
#     author**, and they are recorded here so the next reader knows which
#     witnesses exist: the empty-`CLI_BIN` refusal (the control's test target
#     renamed) and the control finding no `AF_INET`. Both fire.
set -euo pipefail

cd "$(dirname "$0")/.."

# The library, and nothing else. This is the publishable set — what a downstream
# actually links — spelled the same way `just msrv` spells it.
PACKAGES=(tf_tree tf_tree_core tf_tree_math tf_tree_arena tf_tree_ipc)
FEATURES=tf_tree/shm,tf_tree_arena/shm

OUT=${CARGO_TARGET_DIR:-target}/no-network
rm -rf "$OUT"
mkdir -p "$OUT"

# **Refuse rather than skip.** `docs/PROJECT.md` §6 and
# `docs/benchmarks/EVIDENCE.md` are both about the same failure: a check that
# quietly finds nothing reads as coverage. A missing `strace` is not a passing
# run of this assertion, it is the absence of one.
if ! command -v strace >/dev/null 2>&1; then
    echo "no-network: REFUSING — strace is not installed." >&2
    echo "  PHASE5 §5.1's assertion needs it (or a seccomp supervisor, which this" >&2
    echo "  repository does not have). Install it: apt-get install -y strace." >&2
    echo "  This is a refusal and not a skip: a green run here is a claim about" >&2
    echo "  what the library does, and this host cannot make it." >&2
    exit 1
fi

# Print every family a `socket(2)` line names, one per line, over one or more
# strace logs. `-f` prefixes a pid, and a call interrupted by another thread is
# printed as `socket(AF_UNIX, ... <unfinished ...>` — the family is on that half
# either way, which is why this reads the first argument rather than the return.
families() {
    sed -n 's/^[0-9]* *socket(\([A-Z0-9_a-z]*\).*/\1/p' "$@"
}

# **The instrument is checked on every run, against a real `AF_INET` socket.**
#
# `bash`'s `/dev/tcp` redirection issues `socket(AF_INET, SOCK_STREAM,
# IPPROTO_TCP)` before it connects, so port 9 refusing the connection is fine —
# the syscall has already happened. No compiler, no helper binary and no
# language beyond the one this script is written in.
#
# What it rules out is the whole class this check could fail silently in:
# `strace` present but unable to `ptrace` (a container without
# `CAP_SYS_PTRACE`, `yama/ptrace_scope` set to 3, a seccomp policy), or a
# scanner whose pattern has stopped matching `strace`'s output format.
self_check() {
    # Deliberately *not* under "$OUT/*.strace": the scan below globs that,
    # and this run opens a real AF_INET socket on purpose.
    local log="$OUT/self-check/trace"
    mkdir -p "$OUT/self-check"
    strace -f -e trace=socket -o "$log" \
        bash -c 'exec 3<>/dev/tcp/127.0.0.1/9' >/dev/null 2>&1 || true
    if ! families "$log" 2>/dev/null | grep -qx AF_INET; then
        echo "no-network: REFUSING — the instrument did not see a socket it was" >&2
        echo "  shown on purpose. A bash /dev/tcp redirection issues" >&2
        echo "  socket(AF_INET, ...) and this run's scanner found no AF_INET in:" >&2
        echo "    $log" >&2
        echo "  So either strace cannot trace on this host (ptrace_scope, a" >&2
        echo "  container without CAP_SYS_PTRACE, a seccomp policy) or strace's" >&2
        echo "  output format has moved. Either way the scan below would have" >&2
        echo "  passed on a library full of AF_INET sockets." >&2
        exit 1
    fi
}
self_check

# The binaries to trace, from cargo's own build metadata rather than a glob over
# `target/debug/deps` — a stale hash there is how a check comes to be run
# against last week's build.
#
# `kind == "bin"` is dropped: those are helper *children*
# (`tf_tree_rendezvous_child`, `tf_tree_ipc_child`), which the tests spawn and
# `strace -f` therefore already follows. Running one directly with no argument
# would trace a usage message.
pkg_args=()
for p in "${PACKAGES[@]}"; do pkg_args+=(-p "$p"); done
mapfile -t BINARIES < <(
    cargo nextest list "${pkg_args[@]}" --features "$FEATURES" \
        --list-type binaries-only --message-format json 2>/dev/null \
    | tail -1 \
    | python3 -c '
import json, sys
for v in json.load(sys.stdin)["rust-binaries"].values():
    if v["kind"] != "bin":
        print(v["binary-path"])
'
)
if [ "${#BINARIES[@]}" -eq 0 ]; then
    echo "no-network: REFUSING — cargo nextest listed no test binary for" >&2
    echo "  ${PACKAGES[*]}. Nothing was traced, so nothing was asserted." >&2
    exit 1
fi

# `--test-threads 1` because these binaries are being run **outside** nextest,
# which gives every test its own process. Several rendezvous tests share a
# runtime directory and a lock file and are only isolated by that; run
# concurrently in one process, six of them fail for reasons that have nothing to
# do with sockets. One thread restores the property they were written against.
status=0
for b in "${BINARIES[@]}"; do
    n=$(basename "$b")
    if ! strace -f -e trace=socket -o "$OUT/$n.strace" "$b" --test-threads 1 \
            >"$OUT/$n.log" 2>&1; then
        echo "no-network: $n exited non-zero — its output is in $OUT/$n.log" >&2
        status=1
    fi
done
if [ "$status" -ne 0 ]; then
    echo "no-network: REFUSING — a traced binary failed, so its remaining tests" >&2
    echo "  never ran and the sockets they would have opened were never seen." >&2
    exit 1
fi

total=$(families "$OUT"/*.strace | grep -c . || true)
unix=$(families "$OUT"/*.strace | grep -cx AF_UNIX || true)
others=$(families "$OUT"/*.strace | grep -vx AF_UNIX | sort | uniq -c || true)

echo "no-network: ${#BINARIES[@]} library test binaries traced, $total socket(2) call(s), $unix AF_UNIX"

if [ -n "$others" ]; then
    echo >&2
    echo "no-network: FAIL — PHASE5 §5.1: the library opened a socket that is not AF_UNIX." >&2
    echo "$others" >&2
    echo >&2
    echo "  The offending calls, with the binary that made them:" >&2
    for f in "$OUT"/*.strace; do
        grep -n '^[0-9]* *socket(' "$f" | grep -v 'socket(AF_UNIX' \
            | sed "s|^|    $(basename "$f" .strace): |" >&2 || true
    done
    exit 1
fi

# **A run that never opened the socket this claim is about asserts nothing**,
# and that is the shape this check would drift into: drop `shm` from the feature
# set, or let the rendezvous tests stop running, and "no non-AF_UNIX socket"
# becomes a statement about a suite that opens no rendezvous.
#
# **The floor names the binary rather than counting.** A count is the obvious
# guard and it does not work: with `shm` dropped the total falls to 23 rather
# than to 0, because `tf_tree_ipc`'s own tests open theirs whatever
# `tf_tree` was built with — measured, after a bare count was written here and
# the run seeded to defeat it passed. `crates/tf_tree/tests/rendezvous.rs`
# carries `required-features = ["shm"]`, so it is exactly the target that
# disappears, and requiring *it* to have opened an `AF_UNIX` socket is the
# smallest statement that the traced build was the one the claim is about.
rendezvous_trace=$(ls "$OUT"/rendezvous-*.strace 2>/dev/null | head -1 || true)
if [ -z "$rendezvous_trace" ]; then
    echo "no-network: REFUSING — no \`tf_tree::rendezvous\` binary was traced, so" >&2
    echo "  the Phase 2 rendezvous — the one socket this claim is about — was" >&2
    echo "  never opened. That target carries required-features = [\"shm\"];" >&2
    echo "  check that --features $FEATURES still compiles it in." >&2
    exit 1
fi
if [ "$(families "$rendezvous_trace" | grep -cx AF_UNIX || true)" -eq 0 ]; then
    echo "no-network: REFUSING — $(basename "$rendezvous_trace") opened no AF_UNIX" >&2
    echo "  socket. The rendezvous is a socket; a run of its own tests that made" >&2
    echo "  none did not exercise it, and this scan is then about nothing." >&2
    exit 1
fi
# **There was a third check here — `if [ "$unix" -eq 0 ]` — and it was DEAD.**
# It could not fire: the two checks above already refuse unless
# `rendezvous-*.strace` contributed at least one `AF_UNIX` line, and `$unix`
# counts over every trace including that one, so `unix >= 1` is established
# before it is reached. It is deleted rather than moved, because the rendezvous
# floor is the strictly stronger statement (a named binary opened the socket,
# not merely that *something* did) and a second check that can only fire where
# the first already has is a check that cannot fail.
#
# **The case it was written for is still refused, measured rather than argued.**
# Under a stub `strace` that passes `self_check` and truncates every other log —
# so the run observes `0 socket(2) call(s), 0 AF_UNIX` — this script exits 1 at
# *"rendezvous-….strace opened no AF_UNIX socket"*. Removed 2026-09-04, after a
# review seeded every arm in this file and this was the one that would not go
# red.

# **The exception, asserted rather than assumed.**
#
# Everything above is a negative result, and a negative result over the wrong
# set looks exactly like one over the right set. `tf_tree top --web` is the one
# `AF_INET` user this repository has, `crates/tf_tree_cli/tests/web.rs` is the
# target that exercises it, and finding its sockets is what establishes that the
# scan above was pointed at something and would have reported a violation if one
# were there. It is a **separate strace run over a package deliberately outside
# the claim**, not an exception inside the scanner.
CLI_BIN=$(
    cargo nextest list -p tf_tree_cli --list-type binaries-only --message-format json 2>/dev/null \
    | tail -1 \
    | python3 -c '
import json, sys
for k, v in json.load(sys.stdin)["rust-binaries"].items():
    if k == "tf_tree_cli::web":
        print(v["binary-path"])
'
)
if [ -z "$CLI_BIN" ]; then
    echo "no-network: REFUSING — cargo nextest listed no \`tf_tree_cli::web\` binary," >&2
    echo "  so the positive control could not be run. Either that test target was" >&2
    echo "  renamed, in which case fix this script, or \`tf_tree top --web\` no" >&2
    echo "  longer has a test, in which case fix that." >&2
    exit 1
fi
mkdir -p "$OUT/control"
strace -f -e trace=socket -o "$OUT/control/web" "$CLI_BIN" --test-threads 1 \
    >"$OUT/control/web.log" 2>&1 || true
control_inet=$(families "$OUT/control/web" | grep -cx AF_INET || true)
if [ "$control_inet" -eq 0 ]; then
    echo "no-network: REFUSING — the positive control found no AF_INET socket in" >&2
    echo "  \`tf_tree_cli::web\`, which binds one by construction" >&2
    echo "  (crates/tf_tree_cli/src/web.rs). A scan that cannot see the one" >&2
    echo "  network socket this repository is known to have proves nothing about" >&2
    echo "  the library not having any. Trace is in $OUT/control/web." >&2
    exit 1
fi
echo "no-network: control — tf_tree_cli::web opened $control_inet AF_INET socket(s), as it must (§7's web-view amendment)"

echo "no-network: PASS — every socket(2) in the library suite named AF_UNIX (PHASE5 §5.1)"
