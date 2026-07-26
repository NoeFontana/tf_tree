#!/usr/bin/env bash
# The CMake package, end to end — docs/PHASE4.md §4.4.
#
# Configure, build, install, then build a **separate downstream project** that
# reaches tf_tree only through `find_package(tf_tree CONFIG)`. The consumer sets
# no include path, links no system library and names no C++ standard: all three
# have to arrive through the imported target's INTERFACE properties, which is
# the entire claim §4.4 makes.
#
# Building the package alone would not test any of that. Three real defects here
# were invisible until a consumer existed:
#
#   * `add_custom_target` without `ALL` — the imported targets are IMPORTED, so
#     `add_dependencies` on them fires only when something in the same project
#     links them, and nothing does. `cmake --build` built nothing and the
#     install shipped three headers and no library.
#   * `install(FILES ... )` guarded by `EXISTS` — evaluated at configure time,
#     before cargo has run, so it was always false.
#   * `configure_package_config_file` without `PATH_VARS` — `@PACKAGE_<var>@`
#     is only rewritten for variables named there, so the shipped config had an
#     empty include directory and failed in the *consumer*.
set -euo pipefail

cd "$(dirname "$0")/../../../.."   # workspace root
ROOT=$PWD
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "  configure + build + install"
cmake -S "$ROOT/crates/tf_tree_c" -B "$WORK/build" \
      -DCMAKE_INSTALL_PREFIX="$WORK/prefix" -DCMAKE_BUILD_TYPE=Release >"$WORK/log" 2>&1
cmake --build "$WORK/build" -j"$(nproc)" >>"$WORK/log" 2>&1
cmake --install "$WORK/build" >>"$WORK/log" 2>&1

for f in include/tf_tree.h include/tf_tree.hpp include/tf_tree_unstable.h \
         lib/libtf_tree_c.a lib/libtf_tree_c.so \
         lib/cmake/tf_tree/tf_treeConfig.cmake; do
    if [ ! -f "$WORK/prefix/$f" ]; then
        echo "  FAIL: $f was not installed" >&2
        tail -30 "$WORK/log" >&2
        exit 1
    fi
done
echo "  installed 6/6 expected artifacts"

# The .so must carry a SONAME, or a consumer records the absolute build-time
# path in DT_NEEDED and the prefix cannot be moved or packaged. A rustc cdylib
# has none by default; the build passes `-Wl,-soname` for exactly this.
if command -v readelf >/dev/null; then
    if ! readelf -d "$WORK/prefix/lib/libtf_tree_c.so" | grep -q SONAME; then
        echo "  FAIL: the installed .so has no SONAME; the prefix is not relocatable" >&2
        exit 1
    fi
    echo "  .so carries a SONAME"
fi

echo "  downstream find_package(tf_tree CONFIG)"
cmake -S "$ROOT/crates/tf_tree_c/tests/cmake_consumer" -B "$WORK/consumer" \
      -DCMAKE_PREFIX_PATH="$WORK/prefix" >>"$WORK/log" 2>&1
cmake --build "$WORK/consumer" >>"$WORK/log" 2>&1
"$WORK/consumer/consumer"
# The consumer above links the STATIC target. Link the SHARED one too — they
# are separate imported targets resolved by separate find_library calls, and
# a config that gets one right can get the other wrong. It did: both calls
# used `NAMES tf_tree_c`, which on Linux matches the .so first, so the
# "static" target was a shared library wearing a static label.
echo "  downstream, shared target"
sed 's/tf_tree::tf_tree_static/tf_tree::tf_tree/' \
    "$ROOT/crates/tf_tree_c/tests/cmake_consumer/CMakeLists.txt" >"$WORK/shared_CMakeLists.txt"
mkdir -p "$WORK/shared_src"
cp "$WORK/shared_CMakeLists.txt" "$WORK/shared_src/CMakeLists.txt"
cp "$ROOT/crates/tf_tree_c/tests/cmake_consumer/main.cpp" "$WORK/shared_src/"
cmake -S "$WORK/shared_src" -B "$WORK/shared_build" \
      -DCMAKE_PREFIX_PATH="$WORK/prefix" >>"$WORK/log" 2>&1
cmake --build "$WORK/shared_build" >>"$WORK/log" 2>&1
LD_LIBRARY_PATH="$WORK/prefix/lib" "$WORK/shared_build/consumer"

echo "  cmake-check: OK"
