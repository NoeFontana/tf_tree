// C++ wrapper overhead against the raw C ABI — docs/PHASE4.md §7, rows 3-5.
//
// **Gate criterion 2: the wrapper must be within 2 % of the C ABI.** It is
// inline code over an `extern "C"` call, so anything more means it is not
// inline — a copy, an allocation, or a layout conversion that should not exist.
//
// **Built in BOTH error modes, and that is not optional.** This file used to
// compile only with exceptions, because it assigned `plan.at<T>()` straight to
// a `T` — which is the exceptions-mode return type. The `-fno-exceptions` mode
// returns `expected<T>`, and its first implementation stored an `Error` by
// value whose constructor calls `tft_last_error`: one extra FFI call per
// successful lookup, putting that mode at 1.064x against the 1.02 gate while
// this benchmark measured 1.002x and reported a pass. A gate that can only see
// the configuration that happens to be fine is not a gate.
//
// Run pinned; unpinned runs migrate cores and swing by more than the gate:
//   taskset -c 2 <this binary>

#include "tf_tree.hpp"

#include <algorithm>
#include <chrono>
#include <cstdio>
#include <vector>

extern "C" {
tft_status tft_test_tree_create(tft_tree** out);
}

namespace {

constexpr std::size_t N = 4096;
constexpr int ROUNDS = 41;

double median(std::vector<double> v)
{
    std::sort(v.begin(), v.end());
    return v[v.size() / 2];
}

template <typename F>
double one_round(F&& run)
{
    const auto t0 = std::chrono::steady_clock::now();
    const double sink = run();
    const auto t1 = std::chrono::steady_clock::now();
    asm volatile("" : : "r"(&sink) : "memory");
    return std::chrono::duration<double, std::nano>(t1 - t0).count() / static_cast<double>(N);
}

template <typename F>
double bench(F&& run)
{
    for (int i = 0; i < 8; ++i) {
        (void)run();
    }
    std::vector<double> samples;
    samples.reserve(ROUNDS);
    for (int i = 0; i < ROUNDS; ++i) {
        samples.push_back(one_round(run));
    }
    return median(std::move(samples));
}

/// Two paths, **interleaved within each round**, reporting the median of the
/// per-round *ratios* rather than the ratio of two medians.
///
/// This is the difference between a gate that works and one that does not.
/// §7 gate 2 allows 2 %, and on this host the run-to-run spread of two
/// separately-timed loops is around 4 % — frequency scaling, thermal drift and
/// scheduler noise all move both loops, but not at the same moment. Timing them
/// back to back in the same round makes that common-mode, and the ratio becomes
/// stable to well under the gate.
///
/// The first version compared medians of separate loops and produced 0.948,
/// 1.001, 1.002 for the same binary — straddling the gate in both directions.
template <typename A, typename B>
double ratio_of(A&& baseline, B&& candidate)
{
    for (int i = 0; i < 8; ++i) {
        (void)baseline();
        (void)candidate();
    }
    std::vector<double> ratios;
    ratios.reserve(ROUNDS);
    for (int i = 0; i < ROUNDS; ++i) {
        // Alternate the order every round so neither path always pays for
        // warming the caches the other then finds warm.
        double b, c;
        if (i % 2 == 0) {
            b = one_round(baseline);
            c = one_round(candidate);
        } else {
            c = one_round(candidate);
            b = one_round(baseline);
        }
        ratios.push_back(c / b);
    }
    return median(std::move(ratios));
}

}  // namespace

int main()
{
    tft_tree* raw = nullptr;
    if (tft_test_tree_create(&raw) != TFT_OK) {
        std::fprintf(stderr, "fixture failed\n");
        return 1;
    }
    tf_tree::Tree tree = tf_tree::Tree::adopt(raw);
    // No std::move: the result is a prvalue and moving it prevents copy elision.
#ifdef TF_TREE_NO_EXCEPTIONS
    auto plan_r = tree.plan("map", "sensor");
    if (!plan_r) {
        std::fprintf(stderr, "plan failed\n");
        return 1;
    }
    tf_tree::Plan plan = std::move(*plan_r);
#else
    tf_tree::Plan plan = tree.plan("map", "sensor");
#endif

    std::vector<std::int64_t> stamps(N);
    for (std::size_t i = 0; i < N; ++i) {
        stamps[i] = static_cast<std::int64_t>(10000000 + ((i * 7919) % 600000000));
    }

    std::printf("C++ wrapper overhead — PHASE4 §7  [%s]\n",
#ifdef TF_TREE_NO_EXCEPTIONS
                "-fno-exceptions"
#else
                "exceptions"
#endif
    );
    std::printf("================================\n");
    std::printf("%zu lookups/round, median of %d rounds, depth 3\n\n", N, ROUNDS);

    // --- single lookup: raw C ABI vs the wrapper, same layout, same buffer ---
    Eigen::Isometry3d sink_c;
    auto c_path = [&] {
        double acc = 0.0;
        for (std::size_t i = 0; i < N; ++i) {
            const tft_status s =
                tft_plan_at(plan.raw(), stamps[i], TFT_LAYOUT_MAT4_COL, &sink_c);
            (void)s;
            acc += sink_c(0, 3);
        }
        return acc;
    };

    auto cpp_path = [&] {
        double acc = 0.0;
        for (std::size_t i = 0; i < N; ++i) {
#ifdef TF_TREE_NO_EXCEPTIONS
            const auto r = plan.at<Eigen::Isometry3d>(stamps[i]);
            acc += (*r)(0, 3);
#else
            const Eigen::Isometry3d iso = plan.at<Eigen::Isometry3d>(stamps[i]);
            acc += iso(0, 3);
#endif
        }
        return acc;
    };

    const double c_ns = bench(c_path);
    const double cpp_ns = bench(cpp_path);
    // The gate is decided by the interleaved ratio, not by these two medians —
    // see `ratio_of`. They are printed because a reader wants absolute numbers.
    const double ratio = ratio_of(c_path, cpp_path);
    std::printf("%34s %10s\n", "path", "ns/lookup");
    std::printf("%34s %10.1f\n", "tft_plan_at (C ABI)", c_ns);
    std::printf("%34s %10.1f\n", "plan.at<Eigen::Isometry3d>", cpp_ns);
    std::printf("\n  ratio %.3fx   (gate: < 1.02)\n  %s\n", ratio,
                ratio < 1.02 ? "PASS" : "FAIL — see docs/PHASE4.md §0.0");

    // --- batch into Eigen: zero copy, no stride (sizeof == payload) ---
    std::vector<Eigen::Isometry3d> out(N);
    const double batch_ns = bench([&] {
        plan.at_many(stamps.data(), N, out.data());
        return out[0](0, 3);
    });
    std::printf("\n%34s %10.1f\n", "at_many<Eigen::Isometry3d>", batch_ns);
    std::printf("  %.3fx a single lookup — the boundary is paid once per call\n",
                batch_ns / c_ns);

#ifdef TF_TREE_HAS_SOPHUS
    // --- §7 row 5: the strided write against a packed one ---
    //
    // sizeof(Sophus::SE3d) is 64 here against a 56-byte payload, so the stride
    // is not optional — this row is what it *costs*, measured against the same
    // batch written packed into a Quat7 array.
    std::vector<Sophus::SE3d> se3(N);
    const double strided_ns = bench([&] {
        plan.at_many(stamps.data(), N, se3.data());
        return se3[0].translation().x();
    });
    std::vector<tf_tree::Quat7> packed(N);
    const double packed_ns = bench([&] {
        plan.at_many(stamps.data(), N, packed.data());
        return packed[0].tx;
    });
    std::printf("\n%34s %10.1f   (sizeof=%zu, payload=56)\n", "at_many<Sophus::SE3d> strided",
                strided_ns, sizeof(Sophus::SE3d));
    std::printf("%34s %10.1f   (sizeof=56, payload=56)\n", "at_many<Quat7> packed", packed_ns);
    std::printf("  the stride costs %+.2f ns/sample — it writes the same 56 bytes\n"
                "  into a wider slot, so this is cache footprint, not work\n",
                strided_ns - packed_ns);
#else
    std::printf("\n  Sophus absent: §7 row 5 (strided vs packed) NOT MEASURED.\n"
                "  run `just cpp-deps` and rebuild.\n");
#endif

    return ratio < 1.02 ? 0 : 1;
}
