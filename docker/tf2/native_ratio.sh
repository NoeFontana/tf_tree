#!/usr/bin/env bash
# Build and run the fair depth-3 ratio: tf2 native C++, tf_tree through its C ABI.
#
# The Rust harness (`crates/tf_tree_bench/src/ratio.rs`) measures the same
# quotient with tf2 behind `tf_tree_tf2_sys`, which charges tf2 the residual FFI
# boundary — ~21 ns / 8% at this depth, per `docs/benchmarks/tf2.md` bias 3 — and
# therefore flatters `tf_tree`. This one reverses it: tf2 pays nothing and
# `tf_tree` pays the C ABI's measured 1.020x, so the result is a conservative
# lower bound.
#
# Three processes' worth of setup, for one reason: `tft_tree_open` **attaches**
# and cannot create (D18, and not something to work around for a benchmark). So
# a Rust owner serves the arena and dumps the identical `.tfstream`, and this
# program attaches to the first and feeds tf2 the second.
set -euo pipefail

ROS_PREFIX="/opt/ros/${ROS_DISTRO:?source a ROS 2 install first}"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
OUT="$TARGET_DIR/native_ratio"
STREAM="$TARGET_DIR/native/fixture.tfstream"
# Short by necessity: the attach socket path must fit `sun_path`'s 108 bytes,
# and a `target/` under a deep checkout does not.
RT="${TF_TREE_RUNTIME_DIR:-/tmp/tft-native-ratio}"
ARENA="${TF_TREE_NAME:-tf2_native}"

mkdir -p "$(dirname "$OUT")" "$RT"

# 1. The Rust side: the C ABI library it links, and the arena owner.
cargo build --release -p tf_tree_c --features shm
cargo build --release -p tf_tree_bench --features shm --bin native_arena

INCLUDES=(-I"$ROS_PREFIX/include" -Icrates/tf_tree_c/include)
for d in "$ROS_PREFIX"/include/*/; do INCLUDES+=(-I"$d"); done

# 2. The C++ side, linked against the same `libtf_tree_c` the ABI check pins.
g++ -std=c++20 -O2 -DNDEBUG -pthread \
    docker/tf2/native_ratio.cpp -o "$OUT" \
    "${INCLUDES[@]}" -DTFT_HAVE_SHM \
    -L"$ROS_PREFIX/lib" -ltf2 -Wl,-rpath,"$ROS_PREFIX/lib" \
    -L"$TARGET_DIR/release" -ltf_tree_c -Wl,-rpath,"$TARGET_DIR/release" \
    -Wno-deprecated-declarations

# 3. Serve the arena, run the harness against it, then stop serving. The owner
#    holds the arena open until its stdin closes, so the consumer can never
#    outlive the mapping it is reading.
export TF_TREE_RUNTIME_DIR="$RT" TF_TREE_NAME="$ARENA"
coproc OWNER { "$TARGET_DIR/release/native_arena" --name "$ARENA" --stream "$STREAM"; }
# shellcheck disable=SC2154
read -r -u "${OWNER[0]}" line || { echo "the arena owner exited before it was ready" >&2; exit 1; }
case "$line" in
  ready\ *) : ;;
  *) echo "unexpected owner greeting: $line" >&2; exit 1 ;;
esac

status=0
"$OUT" "$STREAM" "$@" || status=$?

# Closing the coproc's stdin is what tells the owner to release the arena.
#
# Guarded, and `|| true` on every step: bash unsets the fd array when a coproc
# has already exited, so a bare `exec {OWNER[1]}>&-` fails with "ambiguous
# redirect" and — under `set -e` — takes the script down *before* `exit
# "$status"` runs. An owner that died mid-run would then be reported as a
# generic 1 instead of the harness's real result, which is the one number this
# script exists to return.
if [ -n "${OWNER[1]:-}" ]; then
    exec {OWNER[1]}>&- || true
fi
wait "${OWNER_PID:-}" 2>/dev/null || true
exit "$status"
