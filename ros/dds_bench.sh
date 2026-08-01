#!/usr/bin/env bash
# The `docs/PHASE5.md` §9.1 end-to-end comparison, orchestrated.
#
# **Runs inside `docker/tf2`.** `just dds-bench` is the entry point.
#
# One publisher, three arms, identical everything else:
#
#   tf2.processes    N separate processes, each a `tf2_ros::TransformListener`
#                    and its own `Buffer` over DDS. The ordinary ROS deployment,
#                    and what the memory and CPU claims are about.
#   tf2.composed     one process, one listener, N query threads. tf2's BEST case,
#                    here so the comparison has a control and cannot be read as
#                    a strawman.
#   tf_tree.composed one process hosting the ingest bridge (§5.8 form 3) with N
#                    query threads on the arena it fills.
#
# There is deliberately no `tf_tree.processes` arm; `dds_report` prints why in
# the table itself, every run, rather than leaving it in a comment.
#
# §9.3's honesty rules are discharged mechanically rather than by care:
#   * identical data — one publisher, generated from one workload entry;
#   * identical QoS — §5.2's, set in `tf_publisher.cpp` and read back by the
#     bridge, which logs the *negotiated* QoS;
#   * identical executor configuration and query schedule — the arms are the
#     same executable with a different `--mode`;
#   * warm-up discarded and stated — `--warmup`, reported in the output;
#   * the RMW, the distro and the DDS version — recorded by
#     `runstore::Run::begin`'s provenance block.
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
rm -rf "$RES"
mkdir -p "$CFG" "$RES"

BIN=$ROOT/target/ros/install/tf_tree_bench_ros/lib/tf_tree_bench_ros
if [ ! -x "$BIN/bench_consumer" ]; then
    echo "dds_bench: $BIN/bench_consumer is missing — run ./ros/build.sh first" >&2
    exit 1
fi

echo "==> generating the workload's publisher plan, bridge config and query set"
cargo run --release -q -p tf_tree_bench --bin dds_report -- \
    emit-config --workload "$WORKLOAD" --out "$CFG"

# The publisher must outlive every arm's warm-up plus its measured window, with
# margin. A publisher that exits first turns the tail of a run into a
# measurement of an idle topic — silently, because both engines keep answering
# from cache for a while.
ARMS=3
PUB_SECONDS=$(python3 -c "print(($WARMUP + $SECONDS_MEASURED + 4) * $ARMS + 5)")

# `set -u` off across the sourcing: ROS's own setup scripts read
# `AMENT_TRACE_SETUP_FILES` unguarded and abort under `nounset`. Restored
# immediately, because everything below this is ours.
set +u
source "/opt/ros/${ROS_DISTRO:-lyrical}/setup.bash"
source "$ROOT/target/ros/install/setup.bash"
set -u

echo "==> ROS ${ROS_DISTRO:-unknown}, RMW ${RMW_IMPLEMENTATION:-<rmw default>}"

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
    done
    local failed=0
    for p in "${pids[@]}"; do
        wait "$p" || failed=1
    done
    if [ "$failed" -ne 0 ]; then
        echo "  a consumer in arm $label exited non-zero; its stderr:" >&2
        cat "$RES/$label".*.err >&2
        exit 1
    fi
}

echo "==> publisher for ${PUB_SECONDS}s"
"$BIN/tf_publisher" --plan "$CFG/plan.txt" --seconds "$PUB_SECONDS" \
    > "$RES/publisher.log" 2>&1 &
PUB=$!
trap 'kill $PUB 2>/dev/null || true' EXIT
# Let discovery settle and `/tf_static` be latched before the first consumer
# joins. A consumer that starts inside discovery measures discovery.
sleep 3

run_arm "tf2.processes" tf2 "$CONSUMERS" 1
run_arm "tf2.composed" tf2 1 "$CONSUMERS"
run_arm "tf_tree.composed" tf_tree 1 "$CONSUMERS"

kill $PUB 2>/dev/null || true
wait $PUB 2>/dev/null || true
trap - EXIT

echo
cargo run --release -q -p tf_tree_bench --bin dds_report -- \
    aggregate --dir "$RES" --workload "$WORKLOAD" --json "$OUT/results.json"
