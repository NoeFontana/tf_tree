// Measurement primitives shared by every arm of the §9.1 comparison.
//
// Deliberately a mirror of `crates/tf_tree_bench/src/mp.rs` rather than
// something new, and the mirroring is load-bearing in two places:
//
//   * `Histogram`'s bucketing and its `encode()` wire format are byte-for-byte
//     `mp::Histogram`'s, so the Rust aggregator (`dds_report`) decodes what
//     these nodes print with `Histogram::decode` and no second implementation
//     of quantiles exists to disagree with the first.
//   * `ProcStats` reads `schedstat` and `smaps_rollup` for the reasons `mp.rs`
//     documents at length: `/proc/self/stat`'s utime/stime are 10 ms ticks and
//     report 0.0% for everything measured here, and summed RSS double-counts
//     every shared page, which is precisely the quantity under test.
//
// `RateLoop` is the coordinated-omission fix. Tick `i` is due at
// `t0 + i/rate` whether or not the consumer was ready; a closed loop cannot
// measure latency at all, because a stall reduces the offered load and every
// recorded sample then looks fast.

#ifndef TF_TREE_BENCH_ROS__MEASURE_HPP_
#define TF_TREE_BENCH_ROS__MEASURE_HPP_

#include <chrono>
#include <cstdint>
#include <fstream>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

namespace tf_tree_bench_ros
{

/// Sub-buckets per power of two. 128, matching `mp.rs`: ~0.8% worst-case
/// quantisation error, far below the run-to-run spread of anything here.
constexpr uint32_t kSubBits = 7;
constexpr uint64_t kSub = 1ull << kSubBits;
constexpr size_t kBuckets = (64 - kSubBits) * kSub + kSub;

/// A log-linear latency histogram in nanoseconds, wire-compatible with
/// `tf_tree_bench::mp::Histogram`.
class Histogram
{
public:
  Histogram()
  : counts_(kBuckets, 0) {}

  static size_t bucket(uint64_t v)
  {
    if (v < kSub) {
      return static_cast<size_t>(v);
    }
    // 63 - clz(v), i.e. the index of the most significant set bit.
    const uint32_t msb = 63u - static_cast<uint32_t>(__builtin_clzll(v));
    const uint32_t shift = msb - kSubBits;
    const uint64_t sub = (v >> shift) & (kSub - 1);
    return static_cast<size_t>(shift + 1) * kSub + sub;
  }

  void record(uint64_t ns)
  {
    counts_[bucket(ns)] += 1;
    total_ += 1;
    if (ns > max_) {max_ = ns;}
  }

  uint64_t count() const {return total_;}

  /// `hist <total> <max> <bucket>:<count> ...` — decoded by `Histogram::decode`.
  std::string encode() const
  {
    std::ostringstream s;
    s << "hist " << total_ << ' ' << max_;
    for (size_t i = 0; i < counts_.size(); ++i) {
      if (counts_[i] != 0) {
        s << ' ' << i << ':' << counts_[i];
      }
    }
    return s.str();
  }

private:
  std::vector<uint32_t> counts_;
  uint64_t total_ = 0;
  uint64_t max_ = 0;
};

/// CPU nanoseconds and proportional set size, from `/proc/self`.
struct ProcStats
{
  uint64_t cpu_ns = 0;
  uint64_t pss_kib = 0;

  static ProcStats read()
  {
    ProcStats s;
    // `schedstat` field 1 is time-on-cpu in nanoseconds. `stat`'s utime/stime
    // are USER_HZ ticks of 10 ms, which against a few milliseconds of work
    // reads as a flat zero for every arm.
    std::ifstream sched("/proc/self/schedstat");
    if (sched) {sched >> s.cpu_ns;}

    std::ifstream smaps("/proc/self/smaps_rollup");
    std::string line;
    while (std::getline(smaps, line)) {
      if (line.rfind("Pss:", 0) == 0) {
        std::istringstream f(line.substr(4));
        f >> s.pss_kib;
        break;
      }
    }
    return s;
  }
};

/// A fixed-rate schedule measured against *intended* start times.
class RateLoop
{
public:
  explicit RateLoop(double hz)
  : period_(std::chrono::duration_cast<std::chrono::nanoseconds>(
        std::chrono::duration<double>(1.0 / hz))),
    start_(std::chrono::steady_clock::now()) {}

  /// Sleep until tick `i` is due and return the instant it *was* due — never
  /// the instant this call returned, which is what hides a backlog.
  std::chrono::steady_clock::time_point next_due()
  {
    const auto due = start_ + period_ * tick_;
    ++tick_;
    std::this_thread::sleep_until(due);
    return due;
  }

private:
  std::chrono::nanoseconds period_;
  std::chrono::steady_clock::time_point start_;
  uint64_t tick_ = 0;
};

inline uint64_t ns_since(std::chrono::steady_clock::time_point t)
{
  return static_cast<uint64_t>(
    std::chrono::duration_cast<std::chrono::nanoseconds>(
      std::chrono::steady_clock::now() - t).count());
}

}  // namespace tf_tree_bench_ros

#endif  // TF_TREE_BENCH_ROS__MEASURE_HPP_
