#!/usr/bin/env bash
# Build and run the fair depth-3 ratio: tf2 native C++, tf_tree through its C ABI.
#
# Unlike the Rust harness (`crates/tf_tree_bench/src/ratio.rs`), tf2 pays no FFI
# boundary and `tf_tree` pays its C ABI, so the result is a conservative lower
# bound; the costs are priced in `native_ratio.cpp`'s header.
#
# `tft_tree_open` attaches and cannot create (D18), so a Rust owner serves the
# arena and dumps the `.tfstream` that feeds tf2.
set -euo pipefail

ROS_PREFIX="/opt/ros/${ROS_DISTRO:?source a ROS 2 install first}"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
OUT="$TARGET_DIR/native_ratio"
STREAM="$TARGET_DIR/native/fixture.tfstream"
# Short: the socket path must fit `sun_path`'s 108 bytes.
RT="${TF_TREE_RUNTIME_DIR:-/tmp/tft-native-ratio}"
ARENA="${TF_TREE_NAME:-tf2_native}"

mkdir -p "$(dirname "$OUT")" "$RT"

# 1. The Rust side: the C ABI library it links, and the arena owner.
cargo build --release -p tf_tree_c --features shm
cargo build --release -p tf_tree_bench --features shm --bin native_arena

INCLUDES=(-I"$ROS_PREFIX/include" -Icrates/tf_tree_c/include)
for d in "$ROS_PREFIX"/include/*/; do INCLUDES+=(-I"$d"); done

# 2. The C++ side.
g++ -std=c++20 -O2 -DNDEBUG -pthread \
    docker/tf2/native_ratio.cpp -o "$OUT" \
    "${INCLUDES[@]}" -DTFT_HAVE_SHM \
    -L"$ROS_PREFIX/lib" -ltf2 -Wl,-rpath,"$ROS_PREFIX/lib" \
    -L"$TARGET_DIR/release" -ltf_tree_c -Wl,-rpath,"$TARGET_DIR/release" \
    -Wno-deprecated-declarations

# 3. Serve the arena, run the harness, stop serving (the owner holds the arena
#    until its stdin closes).
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

# Closing stdin releases the arena. Guarded: bash unsets the fd array once the
# coproc has exited, and `set -e` would then skip `exit "$status"`.
if [ -n "${OWNER[1]:-}" ]; then
    exec {OWNER[1]}>&- || true
fi
wait "${OWNER_PID:-}" 2>/dev/null || true
exit "$status"
