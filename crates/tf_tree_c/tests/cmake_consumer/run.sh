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

# --- TFT_HAVE_SHM, both branches --------------------------------------------
#
# `tf_tree.h` hides `tft_tree_open` behind `#if defined(TFT_HAVE_SHM)`, and the
# package decides that macro by probing the resolved library with `nm`
# (`crates/tf_tree_c/CMakeLists.txt`). Until this arm existed **no host recipe
# exercised the probe's positive branch at all** — everything above takes the
# source-build path, which is a plain `cargo build --release` with default
# features, so `shm` is off and the answer is always 0. The only place the
# `=1` case ran was `ros/build.sh`, in the container, where a failure surfaces
# as a `#error` in a ctest two minutes into a colcon build.
#
# **The prebuilt directory is deliberately MIXED**, and that is what makes this
# arm test the probe rather than test that a file was absent. The source-build
# path cannot do it: `_tf_tree_static` there is a path under the build tree that
# does not exist at configure time, so `tf_tree_probe_shm` returns at its
# `NOT EXISTS` guard and `nm` is never run at all —
#
#     tf_tree: .../build/cargo/release/libtf_tree_c.a does not exist at
#              configure time; TFT_HAVE_SHM off for it
#
# — so asserting 0 against that prefix pins "there was no library to look at",
# not "nm looked and found no tft_tree_open". An earlier revision of this arm
# did exactly that and claimed the stronger thing.
#
# One directory holding an **shm `.a` beside a default-features `.so`** fixes
# both halves at once: `nm` runs on each, has to answer 1 for one and 0 for the
# other, and the two answers must land on their own targets. That is also the
# only shape that catches a regression to #142's reviewed defect — one probe
# applied to both targets — which a directory built from a single `cargo build`
# cannot see, because there both answers are 1 either way.
echo "  TFT_HAVE_SHM: nm answers per artifact, on a mixed prebuilt"
MIX=$WORK/mixed
mkdir -p "$MIX"
cargo build --release -q -p tf_tree_c --features bridge,shm --target-dir "$WORK/cargo-shm"
cargo build --release -q -p tf_tree_c --target-dir "$WORK/cargo-plain"
cp "$WORK/cargo-shm/release/libtf_tree_c.a" "$MIX/"
cp "$WORK/cargo-plain/release/libtf_tree_c.so" "$MIX/"
cmake -S "$ROOT/crates/tf_tree_c" -B "$WORK/shm_build" \
      -DTF_TREE_PREBUILT_DIR="$MIX" \
      -DCMAKE_INSTALL_PREFIX="$WORK/shm_prefix" \
      -DCMAKE_BUILD_TYPE=Release >>"$WORK/log" 2>&1
cmake --install "$WORK/shm_build" >>"$WORK/log" 2>&1
CFG=$WORK/shm_prefix/lib/cmake/tf_tree/tf_treeConfig.cmake
# STATIC 1 and SHARED 0, from one directory: the `1` proves `nm` found the
# symbol in a real archive, the `0` proves it *looked* and did not find it in a
# real shared library, and the pair proves the answers are per artifact.
for want in "TF_TREE_HAVE_SHM_STATIC 1" "TF_TREE_HAVE_SHM_SHARED 0"; do
    if ! grep -q "^set($want)" "$CFG"; then
        echo "  FAIL: expected 'set($want)' from the mixed prebuilt" >&2
        echo "        (shm .a beside a default-features .so; the probe must answer per artifact)" >&2
        grep TF_TREE_HAVE_SHM "$CFG" >&2 || true
        exit 1
    fi
done

# And the ordinary case, where both artifacts come from one build: both 1.
echo "  TFT_HAVE_SHM=1 from a --features bridge,shm prebuilt"
cmake -S "$ROOT/crates/tf_tree_c" -B "$WORK/shm_build2" \
      -DTF_TREE_PREBUILT_DIR="$WORK/cargo-shm/release" \
      -DCMAKE_INSTALL_PREFIX="$WORK/shm_prefix" \
      -DCMAKE_BUILD_TYPE=Release >>"$WORK/log" 2>&1
cmake --install "$WORK/shm_build2" >>"$WORK/log" 2>&1
for v in STATIC SHARED; do
    if ! grep -q "^set(TF_TREE_HAVE_SHM_$v 1)" "$CFG"; then
        echo "  FAIL: the prebuilt has tft_tree_open in it, but TF_TREE_HAVE_SHM_$v is not 1" >&2
        grep TF_TREE_HAVE_SHM "$CFG" >&2 || true
        exit 1
    fi
done

# The macro has to reach a *consumer*, which is the whole point: it is an
# INTERFACE property on the imported targets, and an earlier revision set it in
# the build tree only, where no `find_package` consumer could ever see it.
mkdir -p "$WORK/shm_src"
cat >"$WORK/shm_src/main.cpp" <<'EOF'
#include <tf_tree.h>
#if !defined(TFT_HAVE_SHM)
#error "TFT_HAVE_SHM did not reach this consumer, so tft_tree_open is undeclared"
#endif
#include <cstdio>
int main() {
    // Its *address*, not a call: declaring it is what this arm tests, and
    // calling it would need a live arena this harness has no business creating.
    void *fn = reinterpret_cast<void *>(&tft_tree_open);
    std::printf("shm consumer ok: tft_tree_open is declared and linkable (%p)\n", fn);
    return fn == nullptr;
}
EOF
cat >"$WORK/shm_src/CMakeLists.txt" <<'EOF'
cmake_minimum_required(VERSION 3.16)
project(tf_tree_shm_consumer LANGUAGES CXX)
find_package(tf_tree CONFIG REQUIRED)
add_executable(consumer main.cpp)
target_link_libraries(consumer PRIVATE tf_tree::tf_tree_static)
EOF
cmake -S "$WORK/shm_src" -B "$WORK/shm_consumer" \
      -DCMAKE_PREFIX_PATH="$WORK/shm_prefix" >>"$WORK/log" 2>&1
cmake --build "$WORK/shm_consumer" >>"$WORK/log" 2>&1
"$WORK/shm_consumer/consumer"

echo "  cmake-check: OK"
