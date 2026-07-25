#!/usr/bin/env bash
# Build and run the pure-C++ tf2 read-scaling control.
#
# This is the answer to "is your FFI wrapper the bottleneck?" — it contains no
# Rust and no FFI, reads the same .tfstream, and sweeps the same thread counts.
# Compare its numbers against the tf2 column of `just tf2-scaling`.
set -euo pipefail

ROS_PREFIX="/opt/ros/${ROS_DISTRO:?source a ROS 2 install first}"
OUT="${CARGO_TARGET_DIR:-target}/native_scaling"
mkdir -p "$(dirname "$OUT")"

INCLUDES=(-I"$ROS_PREFIX/include")
for d in "$ROS_PREFIX"/include/*/; do INCLUDES+=(-I"$d"); done

g++ -std=c++20 -O2 -DNDEBUG -pthread \
    docker/tf2/native_scaling.cpp -o "$OUT" \
    "${INCLUDES[@]}" \
    -L"$ROS_PREFIX/lib" -ltf2 -Wl,-rpath,"$ROS_PREFIX/lib" \
    -Wno-deprecated-declarations

exec "$OUT" "$@"
