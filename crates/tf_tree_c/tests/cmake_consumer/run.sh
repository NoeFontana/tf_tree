#!/usr/bin/env bash
# The CMake package, end to end (docs/PHASE4.md §4.4): install, then build a
# separate downstream project that reaches tf_tree only via find_package.
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

# The .so must carry a SONAME so the prefix is relocatable.
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
# The SHARED target is resolved by its own find_library call; link it too.
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
# The package sets the macro by probing each library with `nm`. The prebuilt
# directory is deliberately MIXED (shm .a beside a default-features .so), so the
# probe must answer 1 and 0 on their own targets.
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
# STATIC 1 and SHARED 0 from one directory: the answers are per artifact.
for want in "TF_TREE_HAVE_SHM_STATIC 1" "TF_TREE_HAVE_SHM_SHARED 0"; do
    if ! grep -q "^set($want)" "$CFG"; then
        echo "  FAIL: expected 'set($want)' from the mixed prebuilt" >&2
        echo "        (shm .a beside a default-features .so; the probe must answer per artifact)" >&2
        grep TF_TREE_HAVE_SHM "$CFG" >&2 || true
        exit 1
    fi
done

# The ordinary case, both 1, in its own prefix: install(FILES) skips a
# same-size copy, so reusing shm_prefix would re-read the mixed config.
echo "  TFT_HAVE_SHM=1 from a --features bridge,shm prebuilt"
cmake -S "$ROOT/crates/tf_tree_c" -B "$WORK/shm_build2" \
      -DTF_TREE_PREBUILT_DIR="$WORK/cargo-shm/release" \
      -DCMAKE_INSTALL_PREFIX="$WORK/shm_prefix2" \
      -DCMAKE_BUILD_TYPE=Release >>"$WORK/log" 2>&1
cmake --install "$WORK/shm_build2" >>"$WORK/log" 2>&1
CFG=$WORK/shm_prefix2/lib/cmake/tf_tree/tf_treeConfig.cmake
for v in STATIC SHARED; do
    if ! grep -q "^set(TF_TREE_HAVE_SHM_$v 1)" "$CFG"; then
        echo "  FAIL: the prebuilt has tft_tree_open in it, but TF_TREE_HAVE_SHM_$v is not 1" >&2
        grep TF_TREE_HAVE_SHM "$CFG" >&2 || true
        exit 1
    fi
done

# The macro must reach a consumer as an INTERFACE property.
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
      -DCMAKE_PREFIX_PATH="$WORK/shm_prefix2" >>"$WORK/log" 2>&1
cmake --build "$WORK/shm_consumer" >>"$WORK/log" 2>&1
"$WORK/shm_consumer/consumer"

echo "  cmake-check: OK"
