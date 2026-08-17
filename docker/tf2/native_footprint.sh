#!/usr/bin/env bash
# The memory comparison with no binding on either side: native C++ tf2 in its own
# process, native Rust tf_tree in its own.
#
# `just footprint` already runs its two modes as two invocations so neither
# engine's freed chunks can satisfy the other's requests. This does the same
# thing one step further out: the tf2 arm is a C++ program linking only
# `libtf2`, not a Rust binary linking `tf_tree_tf2_sys`, so the process being
# weighed carries no Rust runtime, no Rust allocator and no shim.
#
# Both arms print the same tab-separated keys and are compared by the awk block
# at the end. Both read the same `.tfstream`, which `native_arena --dump-only`
# generates from the same three lines as `fixture::spin_up` and
# `Tf2Fixture::load`.
set -euo pipefail

ROS_PREFIX="/opt/ros/${ROS_DISTRO:?source a ROS 2 install first}"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
OUT="$TARGET_DIR/native_footprint"
STREAM="$TARGET_DIR/native/fixture.tfstream"

mkdir -p "$(dirname "$OUT")" "$(dirname "$STREAM")"

# 1. The fixture, and the tf_tree arm. `--dump-only` writes the stream and
#    exits: this comparison has no arena in it, only a tf_tree process and a tf2
#    process, so there is nothing to serve.
cargo build --release -q --features shm -p tf_tree_bench --bin native_arena --bin footprint
"$TARGET_DIR/release/native_arena" --dump-only --stream "$STREAM" >/dev/null

INCLUDES=(-I"$ROS_PREFIX/include")
for d in "$ROS_PREFIX"/include/*/; do INCLUDES+=(-I"$d"); done

# 2. The tf2 arm. `-O2 -DNDEBUG` matches `native_ratio.sh`; the allocation
#    behaviour under test is libstdc++'s and libtf2's, not the optimiser's.
g++ -std=c++20 -O2 -DNDEBUG -pthread \
    docker/tf2/native_footprint.cpp -o "$OUT" \
    "${INCLUDES[@]}" \
    -L"$ROS_PREFIX/lib" -ltf2 -Wl,-rpath,"$ROS_PREFIX/lib" \
    -Wno-deprecated-declarations

# 3. Two processes, one after the other.
tf_tree_out=$("$TARGET_DIR/release/footprint" mem-tf_tree)
tf2_out=$("$OUT" "$STREAM")

echo "$tf_tree_out"
echo
echo "$tf2_out"
echo

# 4. The comparison. Only the fields both engines can honestly report are
#    quotiented: tf2 has no declared slots, so `bytes_per_slot` has no tf2 side
#    and is printed for tf_tree alone rather than folded into a ratio that would
#    silently compare a declared cost against a stored one.
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
  # `bytes_per_slot` is what tf_tree would cost if its rings were sized to what
  # they hold. Printing the achievable figure next to the achieved one is the
  # whole reason this comparison was built: docs/benchmarks/tf2.md:473 already
  # says tf_tree is 1.56x denser per unit of capacity and that the fixture
  # "hands almost all of that back", and nothing had ever shown both numbers
  # against a native tf2 in one place.
  printf "right-sized, tf_tree would hold %.1f B/sample against tf2 %s -- %.2fx\n", \
         A["bytes_per_slot"], B["bytes_per_sample"], B["bytes_per_sample"] / A["bytes_per_slot"]
  printf "as measured, it holds %s B/sample -- %.2fx. The difference is declared\n", \
         A["bytes_per_sample"], B["bytes_per_sample"] / A["bytes_per_sample"]
  printf "capacity nobody published into, not engine overhead.\n\n"
  # The two instruments disagree in direction, and that is the finding rather
  # than noise. `heap_bytes` is a tie; Pss -- the operator-visible one -- is not,
  # because tf_tree`s arena is one allocation that is ~100%% resident (decision
  # 0021: `alloc_zeroed` above 16-byte alignment falls back to posix_memalign
  # plus an explicit zero-fill that touches every page) while tf2`s many small
  # allocations are not all faulted. `heap_bytes` is exact and deterministic;
  # `pss_kib_delta` is a page-quantised whole-process delta, stable to ~3%.
  if (A["pss_kib_delta"] > B["pss_kib_delta"]) {
    printf "ON Pss -- what an operator sees in top -- tf_tree is WORSE: %s KiB against\n", \
           A["pss_kib_delta"]
    printf "%s KiB, %.2fx. heap_bytes is a tie and Pss is not, because the arena is\n", \
           B["pss_kib_delta"], A["pss_kib_delta"] / B["pss_kib_delta"]
    printf "one allocation that is ~100%% resident. That is decision 0021, and this\n"
    printf "is the row that has to move.\n"
  }
}'
