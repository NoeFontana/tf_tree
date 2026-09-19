#!/usr/bin/env bash
# Memory comparison with no binding on either side: native C++ tf2 and native Rust
# tf_tree, each in its own process. Both arms print the same tab-separated keys,
# compared by the awk block at the end, and read the same `.tfstream`.
set -euo pipefail

ROS_PREFIX="/opt/ros/${ROS_DISTRO:?source a ROS 2 install first}"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
OUT="$TARGET_DIR/native_footprint"
STREAM="$TARGET_DIR/native/fixture.tfstream"

mkdir -p "$(dirname "$OUT")" "$(dirname "$STREAM")"

# 1. The fixture and the tf_tree arm (`--dump-only`: nothing to serve).
cargo build --release -q --features shm -p tf_tree_bench --bin native_arena --bin footprint
"$TARGET_DIR/release/native_arena" --dump-only --stream "$STREAM" >/dev/null

INCLUDES=(-I"$ROS_PREFIX/include")
for d in "$ROS_PREFIX"/include/*/; do INCLUDES+=(-I"$d"); done

# 2. The tf2 arm; flags match `native_ratio.sh`.
g++ -std=c++20 -O2 -DNDEBUG -pthread \
    docker/tf2/native_footprint.cpp -o "$OUT" \
    "${INCLUDES[@]}" \
    -L"$ROS_PREFIX/lib" -ltf2 -Wl,-rpath,"$ROS_PREFIX/lib" \
    -Wno-deprecated-declarations

# 3. Two processes in sequence.
tf_tree_out=$("$TARGET_DIR/release/footprint" mem-tf_tree)
tf2_out=$("$OUT" "$STREAM")

echo "$tf_tree_out"
echo
echo "$tf2_out"
echo

# 4. The comparison. `bytes_per_slot` has no tf2 side (tf2 declares no slots), so
#    it is printed for tf_tree alone.
awk -v a="$tf_tree_out" -v b="$tf2_out" '
BEGIN {
  n = split(a, la, "\n"); for (i = 1; i <= n; i++) { split(la[i], kv, "\t"); A[kv[1]] = kv[2] }
  n = split(b, lb, "\n"); for (i = 1; i <= n; i++) { split(lb[i], kv, "\t"); B[kv[1]] = kv[2] }
  printf "%-22s %14s %14s %10s\n", "", "tf_tree (Rust)", "tf2 (C++)", "ratio"
  printf "%-22s %14s %14s %10.3f\n", "heap_bytes", A["heap_bytes"], B["heap_bytes"], \
         B["heap_bytes"] / A["heap_bytes"]
  printf "%-22s %14s %14s %10.3f\n", "bytes_per_sample", A["bytes_per_sample"], B["bytes_per_sample"], \
         B["bytes_per_sample"] / A["bytes_per_sample"]
  printf "%-22s %14s %14s %10.3f\n", "pss_kib_delta", A["pss_kib_delta"], B["pss_kib_delta"], \
         B["pss_kib_delta"] / A["pss_kib_delta"]
  printf "%-22s %14s %14s %10s\n", "samples_stored", A["samples_stored"], B["samples_stored"], "-"
  printf "%-22s %14s %14s %10s\n", "bytes_per_slot", A["bytes_per_slot"], "n/a", "-"
  printf "\n"
  if (A["samples_stored"] != B["samples_stored"]) {
    printf "REFUSED: the two arms stored %s and %s samples. They are not the same\n", \
           A["samples_stored"], B["samples_stored"]
    printf "         history, so no quotient above means anything. Check that\n"
    printf "         native_arena dump_stream still mirrors fixture::spin_up.\n"
    exit 1
  }
  # `bytes_per_slot` is tf_tree's cost if its rings were sized to what they hold.
  printf "right-sized, tf_tree would hold %.1f B/sample against tf2 %s -- %.2fx\n", \
         A["bytes_per_slot"], B["bytes_per_sample"], B["bytes_per_sample"] / A["bytes_per_slot"]
  printf "as measured, it holds %s B/sample -- %.2fx. The difference is declared\n", \
         A["bytes_per_sample"], B["bytes_per_sample"] / A["bytes_per_sample"]
  printf "capacity nobody published into, not engine overhead.\n\n"
  # The instruments can disagree: the arena is one ~100%% resident allocation
  # (decision 0021), tf2's small allocations are not all faulted.
  if (A["pss_kib_delta"] > B["pss_kib_delta"]) {
    printf "ON Pss -- what an operator sees in top -- tf_tree is WORSE: %s KiB against\n", \
           A["pss_kib_delta"]
    printf "%s KiB, %.2fx. heap_bytes is a tie and Pss is not, because the arena is\n", \
           B["pss_kib_delta"], A["pss_kib_delta"] / B["pss_kib_delta"]
    printf "one allocation that is ~100%% resident. That is decision 0021, and this\n"
    printf "is the row that has to move.\n"
  }
}'
