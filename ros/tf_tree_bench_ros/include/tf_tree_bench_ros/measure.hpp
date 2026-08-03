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
// **The mirroring was broken in `ProcStats`, and the break was the CPU column.**
// `mp.rs`'s `self_cpu_ns` says why in its own doc comment — *"the process-level
// file covers only the main thread"* — and sums `/proc/self/task/*/schedstat`.
// This header read `/proc/self/schedstat`, the process-level file, so it
// measured the main thread alone. Every arm of §9.1 does its work on *other*
// threads: the query threads, rclcpp's spinner, the bridge's ingest thread.
// Measured on this host, two threads burning 4.004 s of CPU over a 2.003 s
// window moved `/proc/self/schedstat` by 0.000336 s. That is the instrument
// reporting the main thread's sleep, and it is why every CPU %/consumer in
// `docs/benchmarks/tf2.md` came out between 0.003 % and 0.012 %.
//
// It matters most for the arm that made it visible: `tf_tree.processes` charges
// a whole bridge *process* to the arm it serves, and a bridge whose ingest
// thread was invisible to this reading would have made that arm look free.
//
// **`CLOCK_PROCESS_CPUTIME_ID` rather than `mp.rs`'s task sum**, which is the
// one place this file deliberately stops mirroring, because the task sum is
// wrong for the shape these nodes have. It is a sum over *live* tasks, and every
// consumer here reads its second sample after joining its query threads — so
// their CPU is not merely missed, it is **subtracted**, and the `uint64_t`
// difference underflows. That is not hypothetical: it is how this was found, in
// an attach process that printed `cpu_ns 18446744073701835266`.
//
// The two measure the same quantity while the threads are alive — 1.9481 s
// against 1.9479 s over a two-thread burn on this host — and diverge completely
// once one exits: 8.4117 s against 0.0004 s.
//
// `mp.rs` carries the same shape and is **not** reachable through it, which was
// checked rather than assumed: every one of its readers — `mp_consumer`'s two
// passes and `load_child`'s two — is single-threaded between its `before` and
// `after`, because that harness runs one process per participant instead of one
// process with N query threads. Its `since` also saturates, so the underflow
// below cannot occur there even if that ever changes; a thread exiting mid-window
// would silently under-report rather than print a nonsense number. If `mp.rs`
// ever grows a threaded consumer, this paragraph is the one to re-read.
//
// `RateLoop` is the coordinated-omission fix. Tick `i` is due at
// `t0 + i/rate` whether or not the consumer was ready; a closed loop cannot
// measure latency at all, because a stall reduces the offered load and every
// recorded sample then looks fast.

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
    // **Every thread, including the ones that have already exited.** See the
    // file header: `/proc/self/schedstat` is the main thread's alone, and
    // `/proc/self/stat`'s utime/stime are USER_HZ ticks of 10 ms, which against
    // a few milliseconds of work reads as a flat zero for every arm.
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

  /// CPU nanoseconds accumulated between `before` and this reading.
  ///
  /// **Saturating**, so that a clock which somehow ran backwards prints a zero
  /// an operator can disbelieve rather than a `uint64_t` underflow that reads
  /// as 18446744073701835266 ns and sorts to the top of the table. That number
  /// is the literal one this file printed before the reading above replaced a
  /// sum over live tasks.
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
