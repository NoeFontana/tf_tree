#!/usr/bin/env bash
# The `docs/PHASE5.md` §9.1 end-to-end comparison. Runs inside `docker/tf2`
# (`just dds-bench`). One publisher, four arms, everything else identical:
#
#   tf2.processes     N processes, each a `TransformListener` + `Buffer` over DDS
#   tf2.composed      one process, one listener, N query threads (tf2's best case)
#   tf_tree.composed  one process hosting the ingest bridge (§5.8 form 3) with
#                     N query threads
#   tf_tree.processes one bridge process publishing a shared arena under
#                     $TF_TREE_NAME plus N read-only attached consumers
#                     (`docs/decisions/0015`)
#
# The bridge runs only during its own arm, and `cleanup` kills every launched
# process on exit, so no arm's CPU contends with another's (§9.3). Arm order is
# fixed and a disclosed confound; `dds_report` prints it. §9.3's other rules are
# mechanical: one publisher/workload, §5.2's QoS, one executable with `--mode`,
# `--warmup` reported, RMW/distro recorded by `runstore::Run::begin`.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT=$PWD

WORKLOAD=${WORKLOAD:-robot}
CONSUMERS=${CONSUMERS:-4}
SECONDS_MEASURED=${SECONDS_MEASURED:-15}
WARMUP=${WARMUP:-3}
HZ=${HZ:-100}

OUT=$ROOT/target/dds-bench
CFG=$OUT/config
RES=$OUT/results
RUNDIR=$OUT/run
rm -rf "$RES" "$RUNDIR"
mkdir -p "$CFG" "$RES" "$RUNDIR"

# The `tf_tree.processes` arm's rendezvous coordinates, shared by both halves
# through the environment. A private runtime directory and a pinned domain keep
# it unreachable from a developer's `tf_tree serve` or a stale run
# (`$TF_TREE_DOMAIN` would otherwise fall back to `$ROS_DOMAIN_ID`).
export TF_TREE_RUNTIME_DIR=$RUNDIR
export TF_TREE_DOMAIN=0
export TF_TREE_NAME=${TF_TREE_NAME:-ddsbench}

# `sockaddr_un.sun_path` holds 108 bytes, so a deep checkout makes the socket
# path unusable; fail here rather than after three arms.
SOCKET_PATH="$TF_TREE_RUNTIME_DIR/$TF_TREE_DOMAIN/$TF_TREE_NAME.sock"
if [ "${#SOCKET_PATH}" -ge 108 ]; then
    echo "dds_bench: the rendezvous socket path is ${#SOCKET_PATH} bytes and sun_path holds" >&2
    echo "  107 plus a NUL: $SOCKET_PATH" >&2
    echo "  The tf_tree.processes arm cannot run from a checkout this deep. Either clone" >&2
    echo "  somewhere shorter or set TF_TREE_RUNTIME_DIR to a short directory." >&2
    exit 1
fi

# The bridge outlives its own window: its consumers start warming up only once
# the rendezvous exists, so their window ends later.
BRIDGE_LINGER=${BRIDGE_LINGER:-6}

BIN=$ROOT/target/ros/install/tf_tree_bench_ros/lib/tf_tree_bench_ros
if [ ! -x "$BIN/bench_consumer" ]; then
    echo "dds_bench: $BIN/bench_consumer is missing — run ./ros/build.sh first" >&2
    exit 1
fi

echo "==> generating the workload's publisher plan, bridge config and query set"
cargo run --release -q -p tf_tree_bench --bin dds_report -- \
    emit-config --workload "$WORKLOAD" --out "$CFG"

# The publisher must outlive every arm and the bridge's linger, or the tail of a
# run measures an idle topic (both engines answer from cache, silently).
ARMS=4
PUB_SECONDS=$(python3 -c \
    "print(($WARMUP + $SECONDS_MEASURED + 4) * $ARMS + $BRIDGE_LINGER + 5)")

# ROS's setup scripts read `AMENT_TRACE_SETUP_FILES` unguarded; drop `nounset`.
set +u
source "/opt/ros/${ROS_DISTRO:-lyrical}/setup.bash"
source "$ROOT/target/ros/install/setup.bash"
set -u

echo "==> ROS ${ROS_DISTRO:-unknown}, RMW ${RMW_IMPLEMENTATION:-<rmw default>}"

# Processes launched and not yet reaped; an orphaned bridge would confound the
# next run.
ARM_PIDS=()

cleanup() {
    trap - EXIT INT TERM
    if [ -n "${PUB:-}" ]; then
        kill "$PUB" 2>/dev/null || true
    fi
    if [ "${#ARM_PIDS[@]}" -ne 0 ]; then
        kill "${ARM_PIDS[@]}" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

run_arm() {
    local label=$1 mode=$2 procs=$3 cons=$4
    echo "==> arm $label: $procs process(es) x $cons consumer(s)"
    local pids=()
    for i in $(seq 0 $((procs - 1))); do
        local extra=()
        if [ "$mode" = "tf_tree" ]; then extra=(--topology "$CFG/topology.toml"); fi
        "$BIN/bench_consumer" --mode "$mode" \
            --queries "$CFG/queries.txt" "${extra[@]}" \
            --consumers "$cons" --hz "$HZ" \
            --seconds "$SECONDS_MEASURED" --warmup "$WARMUP" \
            > "$RES/$label.$i.out" 2> "$RES/$label.$i.err" &
        pids+=($!)
        ARM_PIDS+=($!)
    done
    local failed=0
    for p in "${pids[@]}"; do
        wait "$p" || failed=1
    done
    ARM_PIDS=()
    if [ "$failed" -ne 0 ]; then
        echo "  a consumer in arm $label exited non-zero; its stderr:" >&2
        cat "$RES/$label".*.err >&2
        exit 1
    fi
}

# One bridge process plus N attached consumers, all writing
# `tf_tree.processes.<i>.out` so `dds_report` charges the bridge's cost to this
# arm (it reports `consumers 0`). All N+1 launch together: consumers poll for
# the rendezvous (`--attach-timeout`) and `$BRIDGE_LINGER` covers the offset.
run_processes_arm() {
    local label=$1 procs=$2
    echo "==> arm $label: 1 bridge process + $procs attached consumer process(es)"
    echo "    arena \"$TF_TREE_NAME\" domain $TF_TREE_DOMAIN in $TF_TREE_RUNTIME_DIR"
    local pids=()
    "$BIN/bench_consumer" --mode tf_tree_bridge \
        --queries "$CFG/queries.txt" --topology "$CFG/topology.toml" \
        --hz "$HZ" --seconds "$SECONDS_MEASURED" --warmup "$WARMUP" \
        --linger "$BRIDGE_LINGER" \
        > "$RES/$label.0.out" 2> "$RES/$label.0.err" &
    pids+=($!)
    ARM_PIDS+=($!)
    for i in $(seq 1 "$procs"); do
        "$BIN/bench_consumer" --mode tf_tree_attach \
            --queries "$CFG/queries.txt" \
            --consumers 1 --hz "$HZ" \
            --seconds "$SECONDS_MEASURED" --warmup "$WARMUP" \
            > "$RES/$label.$i.out" 2> "$RES/$label.$i.err" &
        pids+=($!)
        ARM_PIDS+=($!)
    done
    local failed=0
    for p in "${pids[@]}"; do
        wait "$p" || failed=1
    done
    ARM_PIDS=()
    if [ "$failed" -ne 0 ]; then
        echo "  a process in arm $label exited non-zero; its stderr:" >&2
        cat "$RES/$label".*.err >&2
        exit 1
    fi
}

echo "==> publisher for ${PUB_SECONDS}s"
"$BIN/tf_publisher" --plan "$CFG/plan.txt" --seconds "$PUB_SECONDS" \
    > "$RES/publisher.log" 2>&1 &
PUB=$!
# Let discovery settle and `/tf_static` latch before the first consumer joins.
sleep 3

run_arm "tf2.processes" tf2 "$CONSUMERS" 1
run_arm "tf2.composed" tf2 1 "$CONSUMERS"
run_arm "tf_tree.composed" tf_tree 1 "$CONSUMERS"
run_processes_arm "tf_tree.processes" "$CONSUMERS"

kill $PUB 2>/dev/null || true
wait $PUB 2>/dev/null || true
trap - EXIT INT TERM

echo
# `--ros-out` supplies the arms' build facts; `aggregate --json` refuses without it.
cargo run --release -q -p tf_tree_bench --bin dds_report -- \
    aggregate --dir "$RES" --workload "$WORKLOAD" \
    --ros-out "$ROOT/target/ros" --json "$OUT/results.json"
