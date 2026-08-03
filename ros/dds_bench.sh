#!/usr/bin/env bash
# The `docs/PHASE5.md` §9.1 end-to-end comparison, orchestrated.
#
# **Runs inside `docker/tf2`.** `just dds-bench` is the entry point.
#
# One publisher, four arms, identical everything else:
#
#   tf2.processes     N separate processes, each a `tf2_ros::TransformListener`
#                     and its own `Buffer` over DDS. The ordinary ROS deployment,
#                     and what the memory and CPU claims are about.
#   tf2.composed      one process, one listener, N query threads. tf2's BEST case,
#                     here so the comparison has a control and cannot be read as
#                     a strawman.
#   tf_tree.composed  one process hosting the ingest bridge (§5.8 form 3) with N
#                     query threads on the arena it fills.
#   tf_tree.processes ONE bridge process publishing a shared arena under
#                     $TF_TREE_NAME, plus N separate processes that attach to it
#                     read-only and query. §9.1's actual sentence — "one bridge
#                     plus N tf_tree consumers" — and the row this whole project's
#                     central claim is about. `docs/decisions/0015` is what made
#                     it constructible; before that record `dds_report` printed,
#                     above its own table on every run, that it could not exist.
#
# **The bridge process runs only during its own arm.** It is launched by
# `run_processes_arm` and waited on there, so its ingest CPU never contends with
# the tf2 engines it is being compared against — which would break §9.3's
# "identical everything else" in the one direction that flatters tf_tree.
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
RUNDIR=$OUT/run
rm -rf "$RES" "$RUNDIR"
mkdir -p "$CFG" "$RES" "$RUNDIR"

# The `tf_tree.processes` arm's rendezvous coordinates, and the ONLY way its two
# halves find each other: `tft_tree_open()` takes no name argument, so the
# environment selects the arena for the consumer exactly as it does for
# `tf_tree::open()`, and `bench_consumer --mode tf_tree_bridge` reads the same
# variable when it fills `BridgeOptions::arena_name`. One string, one source.
#
# A private runtime directory rather than `$XDG_RUNTIME_DIR`: a developer's
# `tf_tree serve`, a robot, or a previous run of this script that a signal killed
# before its cleanup are all unreachable from here and this is unreachable from
# them, whatever names collide. `$TF_TREE_DOMAIN` is pinned rather than inherited
# because it falls back to `$ROS_DOMAIN_ID`, which colcon and a developer's shell
# both set.
#
# The other three arms never look at any of this — `--mode tf2` has no arena and
# `--mode tf_tree` builds a private heap one — so exporting it here changes
# nothing about them.
export TF_TREE_RUNTIME_DIR=$RUNDIR
export TF_TREE_DOMAIN=0
export TF_TREE_NAME=${TF_TREE_NAME:-ddsbench}

# How long the bridge keeps serving after its own measured window closes.
#
# Its consumers cannot start warming up until the rendezvous exists, so their
# window ends later than the bridge's by however long the bridge took to publish
# it. A bridge that exited on its own schedule would leave the tail of every
# consumer's measured window reading an arena nobody is writing — fast,
# correct-looking, and meaningless.
BRIDGE_LINGER=${BRIDGE_LINGER:-6}

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
#
# Four arms now, and the fourth outlives its consumers by `$BRIDGE_LINGER`
# because its bridge does. Getting this wrong is silent in exactly one
# direction — the publisher exits, the last arm keeps answering from cache, and
# the run reports the arm it happened to schedule last as the fastest.
ARMS=4
PUB_SECONDS=$(python3 -c \
    "print(($WARMUP + $SECONDS_MEASURED + 4) * $ARMS + $BRIDGE_LINGER + 5)")

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

# The `tf_tree.processes` arm: one bridge process plus N attached consumers.
#
# Everything writes `tf_tree.processes.<i>.out`, so `dds_report`'s aggregator —
# which groups by the label parsed out of the file name and sums `cpu_ns` and
# `pss_kib` across every process in a group — puts the bridge's cost in this arm
# and amortizes it over exactly the consumers it serves. The bridge reports
# `consumers 0`, so it adds to the numerator and not the denominator, and there
# is no way to run this arm without paying for it.
#
# **All N+1 launched together**, with no `sleep` between the bridge and its
# consumers: a consumer polls for the rendezvous itself (`--attach-timeout`) and
# only then begins its warm-up, so the two windows line up to within the
# bridge's startup time and `$BRIDGE_LINGER` covers the difference at the end.
# Sleeping here instead would shift the bridge's *measured* window earlier than
# the consumers' by the length of the sleep.
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
    for i in $(seq 1 "$procs"); do
        "$BIN/bench_consumer" --mode tf_tree_attach \
            --queries "$CFG/queries.txt" \
            --consumers 1 --hz "$HZ" \
            --seconds "$SECONDS_MEASURED" --warmup "$WARMUP" \
            > "$RES/$label.$i.out" 2> "$RES/$label.$i.err" &
        pids+=($!)
    done
    local failed=0
    for p in "${pids[@]}"; do
        wait "$p" || failed=1
    done
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
trap 'kill $PUB 2>/dev/null || true' EXIT
# Let discovery settle and `/tf_static` be latched before the first consumer
# joins. A consumer that starts inside discovery measures discovery.
sleep 3

run_arm "tf2.processes" tf2 "$CONSUMERS" 1
run_arm "tf2.composed" tf2 1 "$CONSUMERS"
run_arm "tf_tree.composed" tf_tree 1 "$CONSUMERS"
run_processes_arm "tf_tree.processes" "$CONSUMERS"

kill $PUB 2>/dev/null || true
wait $PUB 2>/dev/null || true
trap - EXIT

echo
cargo run --release -q -p tf_tree_bench --bin dds_report -- \
    aggregate --dir "$RES" --workload "$WORKLOAD" --json "$OUT/results.json"
