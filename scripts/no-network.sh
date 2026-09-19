#!/usr/bin/env bash
# `docs/PHASE5.md` §5.1's NORMATIVE CI test: `socket(2)` is called only with
# `AF_UNIX` by the `tf_tree` **library**'s test binaries, under `strace -f`.
#
# ## PROVES
#
#   * Every `socket(2)` from the five published crates' test binaries (at the
#     feature set below, whole process tree) names `AF_UNIX`.
#   * `crates/tf_tree/tests/rendezvous.rs`'s binary opened an `AF_UNIX` socket.
#   * `strace` can see `socket(2)` and the scanner flags a non-`AF_UNIX` one
#     (`self_check`).
#   * `tf_tree_cli`'s `web` test opens `AF_INET` sockets and this script finds
#     them (the positive control).
#
# ## DOES NOT PROVE
#
#   * Anything about `tf_tree_cli` except that it opens one (`tf_tree top --web`).
#   * Inherited sockets, code paths no test takes, non-Linux targets.
#   * Other features: `shm` is deliberate, without it the rendezvous is compiled
#     out and the floor names the `tf_tree::rendezvous` binary instead of counting.
#   * Any package outside `PACKAGES` and the control.
set -euo pipefail

cd "$(dirname "$0")/.."

# The publishable set, as `just msrv` spells it.
PACKAGES=(tf_tree tf_tree_core tf_tree_math tf_tree_arena tf_tree_ipc)
FEATURES=tf_tree/shm,tf_tree_arena/shm

OUT=${CARGO_TARGET_DIR:-target}/no-network
rm -rf "$OUT"
mkdir -p "$OUT"

# Refuse rather than skip.
if ! command -v strace >/dev/null 2>&1; then
    echo "no-network: REFUSING — strace is not installed." >&2
    echo "  PHASE5 §5.1's assertion needs it (or a seccomp supervisor, which this" >&2
    echo "  repository does not have). Install it: apt-get install -y strace." >&2
    echo "  This is a refusal and not a skip: a green run here is a claim about" >&2
    echo "  what the library does, and this host cannot make it." >&2
    exit 1
fi

# Print every family a `socket(2)` line names, including `<unfinished ...>` lines.
families() {
    sed -n 's/^[0-9]* *socket(\([A-Z0-9_a-z]*\).*/\1/p' "$@"
}

# Checked every run against a real `AF_INET` socket (bash `/dev/tcp`, port 9 may
# refuse): rules out `strace` unable to `ptrace` and a scanner that stopped matching.
self_check() {
    # Not under "$OUT/*.strace": the scan globs that, and this socket is on purpose.
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

# Binaries come from cargo's metadata; `kind == "bin"` helpers are followed by `strace -f`.
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

# `--test-threads 1`: rendezvous tests share a runtime directory and lock file.
# `prlimit --core=1:1` (0057 step 4) stops a pipe core dump from outlasting
# `wait_within(20 s)`; `just shm-check` and `just shm-rendezvous` carry it too.
status=0
for b in "${BINARIES[@]}"; do
    n=$(basename "$b")
    if ! prlimit --core=1:1 -- \
            strace -f -e trace=socket -o "$OUT/$n.strace" "$b" --test-threads 1 \
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

# The floor names the `rendezvous` binary, not a socket count: `tf_tree_ipc`'s
# own tests open sockets even without `shm`.
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

# The exception, asserted: `tf_tree top --web` (`crates/tf_tree_cli/tests/web.rs`)
# is traced separately, outside the claim; its `AF_INET` sockets must be found.
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
