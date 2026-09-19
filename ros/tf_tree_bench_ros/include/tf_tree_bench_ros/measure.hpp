// Measurement primitives shared by every arm of the §9.1 comparison, mirroring
// `crates/tf_tree_bench/src/mp.rs`:
//
//   * `Histogram`'s bucketing and `encode()` are byte-for-byte `mp::Histogram`'s,
//     so `dds_report` decodes them with `Histogram::decode`.
//   * `ProcStats` reports whole-process CPU and `smaps_rollup`'s `Pss` (utime/stime
//     ticks read 0 here; summed RSS double-counts shared pages).
//
// The CPU half is `CLOCK_PROCESS_CPUTIME_ID`, deliberately not `mp.rs`'s sum over
// `/proc/self/task/*/schedstat`: that sums *live* tasks only, and every consumer
// here samples after joining its query threads, so their CPU is subtracted and
// the `uint64_t` difference underflows. `/proc/self/schedstat` is the main
// thread alone. `mp.rs` is single-threaded between its samples and saturates;
// re-read this if it grows a threaded consumer.
//
// `RateLoop` is the coordinated-omission fix: tick `i` is due at `t0 + i/rate`
// whether or not the consumer was ready.

#ifndef TF_TREE_BENCH_ROS__MEASURE_HPP_
#define TF_TREE_BENCH_ROS__MEASURE_HPP_

#include <time.h>

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

/// CPU nanoseconds and proportional set size for the whole process.
struct ProcStats
{
  uint64_t cpu_ns = 0;
  uint64_t pss_kib = 0;

  static ProcStats read()
  {
    ProcStats s;
    // Every thread, including exited ones (see the file header).
    struct timespec t = {0, 0};
    if (::clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &t) == 0) {
      s.cpu_ns = static_cast<uint64_t>(t.tv_sec) * 1000000000ull +
        static_cast<uint64_t>(t.tv_nsec);
    }

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

  /// CPU nanoseconds accumulated between `before` and this reading; saturating,
  /// so a backwards clock prints 0 rather than an underflowed `uint64_t`.
  uint64_t cpu_since(const ProcStats & before) const
  {
    return cpu_ns >= before.cpu_ns ? cpu_ns - before.cpu_ns : 0;
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
