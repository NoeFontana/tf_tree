// C++ wrapper overhead against the raw C ABI — docs/PHASE4.md §7, rows 3-5.
//
// **Gate criterion 2: the wrapper must be within 2 % of the C ABI.** It is
// inline code over an `extern "C"` call, so anything more means it is not
// inline — a copy, an allocation, or a layout conversion that should not exist.
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
double bench(F&& run)
{
    for (int i = 0; i < 8; ++i) {
        (void)run();
    }
    std::vector<double> samples;
    samples.reserve(ROUNDS);
    for (int i = 0; i < ROUNDS; ++i) {
        const auto t0 = std::chrono::steady_clock::now();
        const double sink = run();
        const auto t1 = std::chrono::steady_clock::now();
        asm volatile("" : : "r"(&sink) : "memory");
        samples.push_back(
            std::chrono::duration<double, std::nano>(t1 - t0).count() / static_cast<double>(N));
    }
    return median(std::move(samples));
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
    tf_tree::Plan plan = tree.plan("map", "sensor");

    std::vector<std::int64_t> stamps(N);
    for (std::size_t i = 0; i < N; ++i) {
        stamps[i] = static_cast<std::int64_t>(10000000 + ((i * 7919) % 600000000));
    }

    std::printf("C++ wrapper overhead — PHASE4 §7\n");
    std::printf("================================\n");
    std::printf("%zu lookups/round, median of %d rounds, depth 3\n\n", N, ROUNDS);

    // --- single lookup: raw C ABI vs the wrapper, same layout, same buffer ---
    Eigen::Isometry3d sink_c;
    const double c_ns = bench([&] {
        double acc = 0.0;
        for (std::size_t i = 0; i < N; ++i) {
            const tft_status s =
                tft_plan_at(plan.raw(), stamps[i], TFT_LAYOUT_MAT4_COL, &sink_c);
            (void)s;
            acc += sink_c(0, 3);
        }
        return acc;
    });

    const double cpp_ns = bench([&] {
        double acc = 0.0;
        for (std::size_t i = 0; i < N; ++i) {
            const Eigen::Isometry3d iso = plan.at<Eigen::Isometry3d>(stamps[i]);
            acc += iso(0, 3);
        }
        return acc;
    });

    const double ratio = cpp_ns / c_ns;
    std::printf("%34s %10s\n", "path", "ns/lookup");
    std::printf("%34s %10.1f\n", "tft_plan_at (C ABI)", c_ns);
    std::printf("%34s %10.1f\n", "plan.at<Eigen::Isometry3d>", cpp_ns);
    std::printf("\n  ratio %.3fx   (gate: < 1.02)\n  %s\n", ratio,
                ratio < 1.02 ? "PASS" : "FAIL — the wrapper is not inline");

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
