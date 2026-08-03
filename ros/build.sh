#!/usr/bin/env bash
# Build (and optionally test) `ros/tf_tree_ros` — `docs/PHASE4.md` §5.
#
# **Runs inside `docker/tf2`, not on the host.** There is no `rclcpp` on the
# host; `just ros-build` and `just ros-test` are the entry points and both go
# through `docker/tf2/run.sh`.
#
# Three steps, and the first two are the ones that are easy to get wrong:
#
#   1. `cargo build -p tf_tree_c --features bridge,shm`. **Two default-off
#      features, and a different failure for each** (see `just test`'s comment
#      for why default-off keeps biting):
#
#        * without `bridge` the nine §5 entry points are not in the archive and
#          the colcon link fails with a list of undefined `tft_bridge_*` symbols;
#        * without `shm` there is no `tft_tree_open`, so `docs/decisions/0015`'s
#          `arena_name` has no `tf_tree::Open` behind it, `tft_bridge_create`
#          *refuses* a non-NULL one, and the CMake package leaves `TFT_HAVE_SHM`
#          undefined — which is what `test_shared_arena.cpp`'s `#error` reports,
#          loudly, rather than skipping.
#
#      The symbol check below asks for one symbol from each and says which
#      feature is missing, because "rebuild it" is not actionable without that.
#   2. Configure, build and *install* `crates/tf_tree_c`'s CMake package into a
#      prefix. `find_package(tf_tree CONFIG)` needs the installed
#      `tf_treeConfig.cmake`, which only exists after `cmake --install`;
#      pointing a consumer at the build tree finds nothing. `just cmake-check`
#      is the same sequence with a different consumer.
#   3. `colcon build`, with every output directory named explicitly. colcon
#      otherwise drops `build/`, `install/` and `log/` in whatever directory it
#      was invoked from, which here is the repository root.
#
# `TF_TREE_PREBUILT_DIR` makes step 2 consume step 1's archive rather than
# shelling out to cargo again, which is §4.4's "a ROS shop should not have to
# install rustup first" path exercised for real.
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
# The check that catches a stale archive built without a feature, *before*
# colcon spends two minutes reaching the same conclusion in linker output.
#
# **One symbol per feature, and a message that says which.** `tft_bridge_offer`
# is `bridge`; `tft_tree_open` is `shm`. An operator told only "rebuild it" has
# to go and read this script to find out what to rebuild it with, and the two
# features fail in completely different places — a missing `bridge` is a link
# error, a missing `shm` is a `TFT_HAVE_SHM` that never gets defined and a
# `test_shared_arena.cpp` that will not compile.
#
# Not `nm ... | grep -q`: under `set -o pipefail`, `grep -q` exits on the first
# match, `nm` then dies of SIGPIPE, and the pipeline's status is 141 — so the
# check reported "no tft_bridge_offer" precisely when the symbol *was* there.
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
# `--log-base` is a *global* colcon option and has to precede the verb;
# `colcon build --log-base ...` is an "unrecognized arguments" error. The other
# two bases belong to the verb.
colcon --log-base "$OUT/log" build \
    --base-paths "$ROOT/ros" \
    --build-base "$OUT/build" \
    --install-base "$OUT/install" \
    --cmake-args -DCMAKE_BUILD_TYPE=Release "-DCMAKE_PREFIX_PATH=$PREFIX"

# §5.8 forms 1 and 2 are *artifacts*, and a single `rclcpp_components_register_node`
# call is supposed to produce both. Nothing in the ctests would notice if one of
# them stopped being produced — a component that no longer registers still links,
# still compiles, and still passes every test that constructs `BridgeNode`
# directly. So they are checked here, where they are built.
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
    # **Delete the previous run's result XML first.** `colcon test-result` is the
    # line that decides this script's exit status, and it counts every
    # `*.xml` under `--test-result-base` whether or not this run produced it.
    # The build base is never cleaned, so a test binary that has been *removed*
    # — its `ament_add_gtest` line deleted, its source gone — keeps reporting
    # its last passing rows forever. Measured: after adding a fifth test and
    # then deleting it, `just ros-test` printed "Summary: 15 tests, 0 errors, 0
    # failures" from four binaries. Green, and counting a test that no longer
    # exists. Given this repository's history of tests that ship in no recipe,
    # that is precisely the failure mode this gate is here to prevent.
    rm -rf "$OUT"/build/*/test_results
    # `--event-handlers console_direct+` so gtest's own output reaches the
    # terminal; colcon otherwise buries a failure in a log file nobody reads.
    # `colcon test` exits 0 even when tests fail, which is why `test-result`
    # follows it and is the line that decides the exit status.
    colcon --log-base "$OUT/log" test \
        --base-paths "$ROOT/ros" \
        --build-base "$OUT/build" \
        --install-base "$OUT/install" \
        --event-handlers console_direct+
    colcon test-result --test-result-base "$OUT/build" --verbose
fi

echo "ros/build.sh: OK"
