#!/usr/bin/env bash
# The C++ wrapper's build matrix — docs/PHASE4.md §6.2: -Wall -Wextra -Wpedantic,
# with and without -fno-exceptions, C++17 and C++20, GCC and Clang (8 builds, each
# run), then 4 `--wrap` builds and 1 sanitizer build (§7 gate 4).
# Sophus is optional and its absence is reported; `just cpp-deps` fetches it.
set -euo pipefail

cd "$(dirname "$0")/../../../.."   # workspace root

INC=crates/tf_tree_c/include
SRC=crates/tf_tree_c/tests/cpp/wrapper.cpp
LIB=target/release/libtf_tree_c.a
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

# `-isystem` keeps -Wpedantic -Werror aimed at our code. Eigen is required
# (absence fails); `target/thirdparty/eigen` is the bootstrap copy.
EIGEN=""
for d in /usr/include/eigen3 /usr/local/include/eigen3 target/thirdparty/eigen; do
    [ -d "$d" ] && EIGEN="-isystem $d" && break
done
if [ -z "$EIGEN" ]; then
    echo "cpp-check: FAIL — Eigen not found; §4.2's interop cannot be exercised." >&2
    echo "        run \`just cpp-deps\` to fetch it (header-only), or install" >&2
    echo "        libeigen3-dev / eigen." >&2
    exit 1
fi

SOPHUS=""
if [ -f target/thirdparty/Sophus/sophus/se3.hpp ]; then
    # Sophus 1.22 pulls in fmt unless basic logging is selected.
    SOPHUS="-isystem target/thirdparty/Sophus -DSOPHUS_USE_BASIC_LOGGING"
else
    echo "  note: Sophus absent — §4.3's stride path is NOT exercised."
    echo "        run \`just cpp-deps\` to fetch it (header-only, ~10 MB)."
fi

cargo build --release -q -p tf_tree_c --features test-hooks

WARN="-Wall -Wextra -Wpedantic -Werror"

# `--wrap` records the `out` pointer given to `tft_plan_at` so §7 gate 2's
# `check_at_writes_into_the_returned_object` needs no stopwatch. It needs GNU
# ld/lld, so only the `--wrap` rows below use it, not the §6.2 matrix.
WRAP="-DTF_TREE_WRAP_PLAN_AT -Wl,--wrap=tft_plan_at"

fail=0

for cxx in g++ clang++; do
    # A missing compiler is a failure, not a skip (§6.2 names both).
    if ! command -v "$cxx" >/dev/null; then
        echo "  FAIL: $cxx is not installed; §6.2 requires both compilers." >&2
        fail=1
        continue
    fi
    for std in c++17 c++20; do
        for mode in exceptions no-exceptions; do
            flags=""
            [ "$mode" = "no-exceptions" ] && flags="-fno-exceptions"
            label="$cxx -std=$std $mode"
            printf '  %-34s ' "$label"
            # shellcheck disable=SC2086
            if ! $cxx -std=$std $flags $WARN -I "$INC" $EIGEN $SOPHUS \
                    -o "$OUT/w" "$SRC" "$LIB" -lpthread -ldl -lm 2>"$OUT/err"; then
                echo "COMPILE FAILED"
                sed 's/^/      /' "$OUT/err" | head -20
                fail=1
                continue
            fi
            if ! "$OUT/w" >"$OUT/log" 2>&1; then
                echo "RUN FAILED"
                sed 's/^/      /' "$OUT/log" | head -20
                fail=1
                continue
            fi
            tail -n +2 "$OUT/log" | sed 's/^  /      /' | head -3
        done
    done
done

# §7 gate 2: both compilers and error modes (NRVO varies with them), one `std`.
for cxx in g++ clang++; do
    command -v "$cxx" >/dev/null || continue   # already reported by the matrix
    for mode in exceptions no-exceptions; do
        flags=""
        [ "$mode" = "no-exceptions" ] && flags="-fno-exceptions"
        printf '  %-34s ' "$cxx --wrap $mode"
        # shellcheck disable=SC2086
        if ! $cxx -std=c++17 $flags $WARN $WRAP -I "$INC" $EIGEN $SOPHUS \
                -o "$OUT/w" "$SRC" "$LIB" -lpthread -ldl -lm 2>"$OUT/err"; then
            echo "COMPILE FAILED"
            sed 's/^/      /' "$OUT/err" | head -20
            fail=1
            continue
        fi
        if ! "$OUT/w" >"$OUT/log" 2>&1; then
            echo "RUN FAILED"
            sed 's/^/      /' "$OUT/log" | head -20
            fail=1
            continue
        fi
        echo "ok"
    done
done

# §7 gate 4: zero ASan/UBSan findings; one configuration is enough.
printf '  %-34s ' "clang++ asan+ubsan"
if command -v clang++ >/dev/null; then
    # shellcheck disable=SC2086
    clang++ -std=c++17 -g -fsanitize=address,undefined -fno-omit-frame-pointer \
        $WARN -I "$INC" $EIGEN $SOPHUS -o "$OUT/wsan" "$SRC" "$LIB" \
        -lpthread -ldl -lm 2>"$OUT/err" || { echo "COMPILE FAILED"; head -20 "$OUT/err"; fail=1; }
    if [ -x "$OUT/wsan" ]; then
        if ASAN_OPTIONS=detect_leaks=1 UBSAN_OPTIONS=halt_on_error=1 \
                "$OUT/wsan" >"$OUT/log" 2>&1; then
            echo "clean"
        else
            echo "FINDINGS"
            sed 's/^/      /' "$OUT/log" | head -30
            fail=1
        fi
    fi
else
    echo "FAIL — §7 gate 4 needs clang++ for ASan/UBSan"
    fail=1
fi

[ "$fail" -eq 0 ] && echo "  cpp-check: OK" || { echo "  cpp-check: FAILED" >&2; exit 1; }
