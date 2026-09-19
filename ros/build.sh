#!/usr/bin/env bash
# Build (and optionally test) `ros/tf_tree_ros` — `docs/PHASE4.md` §5.
# Runs inside `docker/tf2` (`just ros-build` / `just ros-test`), not on the host.
#
#   1. `cargo build -p tf_tree_c --features bridge,shm`. Without `bridge` the
#      colcon link fails on `tft_bridge_*`; without `shm` there is no
#      `tft_tree_open` and `TFT_HAVE_SHM` stays undefined (`docs/decisions/0015`).
#   2. Install `crates/tf_tree_c`'s CMake package into a prefix
#      (`find_package(tf_tree CONFIG)` needs the installed config).
#   3. `colcon build` with every output directory named explicitly.
#
# `TF_TREE_PREBUILT_DIR` makes step 2 consume step 1's archive (PHASE4 §4.4).
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT=$PWD
OUT=$ROOT/target/ros
PREFIX=$OUT/prefix

run_tests=0
if [ "${1:-}" = "--test" ]; then
    run_tests=1
fi

mkdir -p "$OUT"

echo "==> 1/3  libtf_tree_c.a --features bridge,shm"
cargo build --release -q -p tf_tree_c --features bridge,shm
LIBDIR="${CARGO_TARGET_DIR:-$ROOT/target}/release"
if [ ! -f "$LIBDIR/libtf_tree_c.a" ]; then
    echo "  FAIL: no $LIBDIR/libtf_tree_c.a" >&2
    exit 1
fi
# Catch a stale archive built without a feature before colcon fails in the
# linker. Not `nm | grep -q`: under pipefail `nm` dies of SIGPIPE (status 141).
symbols=$(nm --defined-only "$LIBDIR/libtf_tree_c.a" 2>/dev/null || true)
case "$symbols" in
    *" T tft_bridge_offer"*) ;;
    *)
        echo "  FAIL: $LIBDIR/libtf_tree_c.a has no tft_bridge_offer." >&2
        echo "        It was built without --features bridge; remove it and re-run." >&2
        exit 1
        ;;
esac
case "$symbols" in
    *" T tft_tree_open"*) ;;
    *)
        echo "  FAIL: $LIBDIR/libtf_tree_c.a has no tft_tree_open." >&2
        echo "        It was built without --features shm, so docs/decisions/0015's" >&2
        echo "        arena_name has nothing behind it and the CMake package will" >&2
        echo "        leave TFT_HAVE_SHM undefined; remove it and re-run." >&2
        exit 1
        ;;
esac

echo "==> 2/3  find_package(tf_tree CONFIG) prefix"
cmake -S "$ROOT/crates/tf_tree_c" -B "$OUT/tf_tree-build" \
      -DCMAKE_BUILD_TYPE=Release \
      -DTF_TREE_PREBUILT_DIR="$LIBDIR" \
      -DCMAKE_INSTALL_PREFIX="$PREFIX" >"$OUT/cmake.log" 2>&1
cmake --build "$OUT/tf_tree-build" -j"$(nproc)" >>"$OUT/cmake.log" 2>&1
cmake --install "$OUT/tf_tree-build" >>"$OUT/cmake.log" 2>&1
if [ ! -f "$PREFIX/lib/cmake/tf_tree/tf_treeConfig.cmake" ]; then
    echo "  FAIL: tf_treeConfig.cmake was not installed; see $OUT/cmake.log" >&2
    tail -30 "$OUT/cmake.log" >&2
    exit 1
fi

echo "==> 3/3  colcon build"
# `--log-base` is a global colcon option and must precede the verb.
colcon --log-base "$OUT/log" build \
    --base-paths "$ROOT/ros" \
    --build-base "$OUT/build" \
    --install-base "$OUT/install" \
    --cmake-args -DCMAKE_BUILD_TYPE=Release "-DCMAKE_PREFIX_PATH=$PREFIX"

# §5.8 forms 1 and 2 are build artifacts no ctest would notice losing; check
# them where they are built.
plugin_index=$OUT/install/tf_tree_ros/share/ament_index/resource_index/rclcpp_components/tf_tree_ros
form1=$OUT/install/tf_tree_ros/lib/tf_tree_ros/tf_tree_bridge
if [ ! -x "$form1" ]; then
    echo "  FAIL: §5.8 form 1 (the standalone executable) was not installed at $form1" >&2
    exit 1
fi
if ! grep -q '^tf_tree_ros::BridgeNode;' "$plugin_index" 2>/dev/null; then
    echo "  FAIL: §5.8 form 2 (the component) is not registered in $plugin_index" >&2
    exit 1
fi
echo "     §5.8 forms 1 and 2 present: tf_tree_bridge, tf_tree_ros::BridgeNode"

if [ "$run_tests" -eq 1 ]; then
    echo "==> ctest"
    # Delete stale result XML: `colcon test-result` counts every `*.xml`, so a
    # removed test keeps reporting its last passing rows.
    rm -rf "$OUT"/build/*/test_results
    # `colcon test` exits 0 on failures; `test-result` decides the exit status.
    colcon --log-base "$OUT/log" test \
        --base-paths "$ROOT/ros" \
        --build-base "$OUT/build" \
        --install-base "$OUT/install" \
        --event-handlers console_direct+
    colcon test-result --test-result-base "$OUT/build" --verbose
fi

echo "ros/build.sh: OK"
