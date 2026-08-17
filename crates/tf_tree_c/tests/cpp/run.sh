#!/usr/bin/env bash
# The C++ wrapper's build matrix — docs/PHASE4.md §6.2.
#
# "Compiles clean under -Wall -Wextra -Wpedantic, with and without
#  -fno-exceptions, C++17 and C++20, GCC and Clang."
#
# That is 2 compilers x 2 standards x 2 error modes = 8 builds, each of which is
# also *run*. Compiling is not the assertion: the wrapper is header-only inline
# code, so a build that compiles and a build that computes the right transform
# are different claims.
#
# Two groups follow the matrix and are not part of it: 4 `--wrap` builds that
# add `check_at_writes_into_the_returned_object` (see the `WRAP` note below for
# why they are kept separate), and 1 sanitizer build. 13 in total.
#
# Sophus is optional and its absence is reported rather than skipped silently —
# §4.3's stride hazard only exists where Sophus does, and a run that did not
# exercise it should not read like one that did. `just cpp-deps` fetches it.
#
# ASan + UBSan across the whole suite is §7 gate criterion 4, and runs last
# because it is the slowest.
set -euo pipefail

cd "$(dirname "$0")/../../../.."   # workspace root

INC=crates/tf_tree_c/include
SRC=crates/tf_tree_c/tests/cpp/wrapper.cpp
LIB=target/release/libtf_tree_c.a
OUT=$(mktemp -d)
trap 'rm -rf "$OUT"' EXIT

# `-isystem`, not `-I`, for both third-party trees. `-Wpedantic -Werror` is
# aimed at *our* header and our test; pointed at Eigen and Sophus it reports
# their choices, and clang duly failed the build on Sophus's use of the GNU
# `##__VA_ARGS__` extension. `-isystem` is how you say "warn about my code".
# The system copy wins over the fetched one, because §4.2 is about interop with
# the Eigen a consumer actually has; `target/thirdparty/eigen` is the bootstrap
# for a machine that has none, which is every GitHub runner. Eigen is NOT
# optional the way Sophus is — its absence fails rather than skips, and on
# 2026-08-17 that is exactly what the first nightly run did, because `cpp-deps`
# fetched Sophus and nothing fetched Eigen.
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
    # Sophus 1.22's common.hpp pulls in fmt unless basic logging is selected;
    # fmt is not otherwise needed and is not worth a dependency for a test.
    SOPHUS="-isystem target/thirdparty/Sophus -DSOPHUS_USE_BASIC_LOGGING"
else
    echo "  note: Sophus absent — §4.3's stride path is NOT exercised."
    echo "        run \`just cpp-deps\` to fetch it (header-only, ~10 MB)."
fi

cargo build --release -q -p tf_tree_c --features test-hooks

WARN="-Wall -Wextra -Wpedantic -Werror"

# **`--wrap` is what makes §7 gate 2 testable without a stopwatch.** It points
# this file's calls to `tft_plan_at` at a shim that records the `out` pointer it
# was handed, so `check_at_writes_into_the_returned_object` can assert that
# `Plan::at<T>` gave the ABI the address of the object it returns rather than a
# temporary it copies out of afterwards. The alternative — a `test-hooks` symbol
# on the Rust side — would put that store inside the function `just cpp-bench`
# times. The shim forwards to `__real_tft_plan_at`, so the rest of the file
# behaves identically under it.
#
# **It is deliberately kept out of the §6.2 matrix below.** `--wrap` is a
# GNU-ld/lld option and Mach-O's `ld64` has no equivalent, so putting it on the
# portability rows would make the repo's C++ *portability* gate require a
# specific linker family in order to test something that has nothing to do with
# portability. The four rows below get it instead, and the eight matrix rows
# stay linker-agnostic.
WRAP="-DTF_TREE_WRAP_PLAN_AT -Wl,--wrap=tft_plan_at"

fail=0

for cxx in g++ clang++; do
    # **A missing compiler is a failure, not a skip.** §6.2 names GCC *and*
    # Clang as the requirement; skipping one silently removed 4 of the 8 rows
    # and left the script printing "cpp-check: OK". Eigen is already a hard
    # failure a few lines up, and this is the same kind of thing.
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

# §7 gate 2, pinned structurally: `check_at_writes_into_the_returned_object`.
#
# Both compilers and both error modes, because that is exactly what the property
# varies with — NRVO is a per-compiler decision, and the error mode is the whole
# of the asymmetry §0.0 records. **Not** both standard revisions: copy elision
# is unchanged between C++17 and C++20, so a second `std` would double the rows
# and hold nothing new. These builds run the whole file, not just the one check,
# which is also what shows that the forwarding shim perturbs nothing.
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

# §7 gate 4: zero ASan/UBSan findings across the C++ suite. One configuration is
# enough — the sanitizers find memory and UB errors, which do not depend on the
# standard revision or the error mode.
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
