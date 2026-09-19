// Ground-truth control: multithreaded `tf2::BufferCore` read scaling in pure C++,
// with no Rust and no FFI. It loads the same `.tfstream`, asks the same queries
// and sweeps the same thread counts as the Rust harness, so agreement shows the
// `tf_tree_tf2_sys` shim is not distorting tf2's threading collapse.
//
// Build and run via `docker/tf2/native_scaling.sh`.

#include <tf2/buffer_core.hpp>
#include <geometry_msgs/msg/transform_stamped.hpp>

#include <algorithm>
#include <atomic>
#include <barrier>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <limits>
#include <map>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

namespace {

struct Sample {
  std::string parent, child;
  std::int64_t stamp_ns;
  double q[4];  // w x y z
  double t[3];
};

struct Stream {
  std::vector<Sample> statics;
  std::vector<Sample> dynamics;
};

/// Parse the same `.tfstream` the Rust side reads. Format:
///   S <parent> <child> qw qx qy qz tx ty tz
///   D <parent> <child> <stamp_ns> qw qx qy qz tx ty tz
Stream load(const std::string &path) {
  Stream s;
  std::ifstream in(path);
  if (!in) {
    std::fprintf(stderr, "cannot open %s\n", path.c_str());
    std::exit(1);
  }
  std::string line;
  while (std::getline(in, line)) {
    if (line.empty() || line[0] == '#') continue;
    std::istringstream f(line);
    std::string kind;
    Sample x;
    f >> kind >> x.parent >> x.child;
    if (kind == "D") f >> x.stamp_ns;
    else x.stamp_ns = 0;
    f >> x.q[0] >> x.q[1] >> x.q[2] >> x.q[3] >> x.t[0] >> x.t[1] >> x.t[2];
    (kind == "S" ? s.statics : s.dynamics).push_back(std::move(x));
  }
  return s;
}

geometry_msgs::msg::TransformStamped to_msg(const Sample &x) {
  geometry_msgs::msg::TransformStamped m;
  m.header.frame_id = x.parent;
  m.child_frame_id = x.child;
  m.header.stamp.sec = static_cast<std::int32_t>(x.stamp_ns / 1000000000LL);
  m.header.stamp.nanosec = static_cast<std::uint32_t>(x.stamp_ns % 1000000000LL);
  m.transform.rotation.w = x.q[0];
  m.transform.rotation.x = x.q[1];
  m.transform.rotation.y = x.q[2];
  m.transform.rotation.z = x.q[3];
  m.transform.translation.x = x.t[0];
  m.transform.translation.y = x.t[1];
  m.transform.translation.z = x.t[2];
  return m;
}

}  // namespace

int main(int argc, char **argv) {
  const std::string path =
      argc > 1 ? argv[1] : "testdata/tfstream/indoor_atelier.tfstream";
  const std::string target = argc > 2 ? argv[2] : "camera_link";
  const std::string source = argc > 3 ? argv[3] : "odom_combined";
  const int rounds = argc > 4 ? std::atoi(argv[4]) : 101;

  Stream s = load(path);
  tf2::BufferCore buf(tf2::durationFromSec(600.0));
  for (const auto &x : s.statics) buf.setTransform(to_msg(x), "native", true);
  for (const auto &x : s.dynamics) buf.setTransform(to_msg(x), "native", false);

  // Query window: identical to the Rust harness's `TfStream::common_window`
  // (latest per-edge first stamp to earliest per-edge last stamp), so no sweep
  // extrapolates on any edge.
  std::int64_t lo = std::numeric_limits<std::int64_t>::min();
  std::int64_t hi = std::numeric_limits<std::int64_t>::max();
  {
    // (parent, child) -> [first, last]: one entry per published edge.
    std::map<std::pair<std::string, std::string>, std::pair<std::int64_t, std::int64_t>> span;
    for (const auto &x : s.dynamics) {
      auto key = std::make_pair(x.parent, x.child);
      auto it = span.find(key);
      if (it == span.end()) {
        span.emplace(key, std::make_pair(x.stamp_ns, x.stamp_ns));
      } else {
        it->second.first = std::min(it->second.first, x.stamp_ns);
        it->second.second = std::max(it->second.second, x.stamp_ns);
      }
    }
    if (span.empty()) {
      std::fprintf(stderr, "%s has no dynamic samples\n", path.c_str());
      std::exit(1);
    }
    for (const auto &e : span) {
      lo = std::max(lo, e.second.first);
      hi = std::min(hi, e.second.second);
    }
    if (lo >= hi) {
      std::fprintf(stderr, "no window is covered by every dynamic edge\n");
      std::exit(1);
    }
  }

  const int per_round = 4096;
  std::vector<std::int64_t> stamps(per_round);
  for (int k = 0; k < per_round; ++k)
    stamps[k] = lo + (hi - lo) * k / per_round;

  std::printf("native C++ tf2 read scaling (no Rust, no FFI)\n");
  std::printf("stream=%s  %s <- %s  %d rounds x %d lookups/thread\n",
              path.c_str(), target.c_str(), source.c_str(), rounds, per_round);
  // Printed so the window can be checked against the Rust harness's.
  std::printf("common window: %.3f s .. %.3f s\n\n", lo / 1e9, hi / 1e9);
  std::printf("%-8s %14s %10s\n", "threads", "tf2 M/s", "vs 1 thread");

  double base = 0.0;
  for (int threads : {1, 2, 4, 8}) {
    std::vector<double> round_rates;
    std::barrier sync(threads);
    std::atomic<bool> stop{false};
    std::atomic<int> go{0};

    auto work = [&]() {
      double acc = 0.0;
      for (auto ns : stamps) {
        try {
          auto r = buf.lookupTransform(target, source,
                                       tf2::TimePoint(std::chrono::nanoseconds(ns)));
          acc += r.transform.translation.x;
        } catch (...) {
        }
      }
      return acc;
    };

    std::vector<std::thread> pool;
    for (int i = 0; i < threads - 1; ++i) {
      pool.emplace_back([&]() {
        for (;;) {
          sync.arrive_and_wait();
          if (stop.load(std::memory_order_acquire)) return;
          volatile double sink = work();
          (void)sink;
          sync.arrive_and_wait();
        }
      });
    }

    for (int w = 0; w < 3; ++w) {  // warm up
      sync.arrive_and_wait();
      volatile double sink = work();
      (void)sink;
      sync.arrive_and_wait();
    }
    for (int r = 0; r < rounds; ++r) {
      sync.arrive_and_wait();
      auto t0 = std::chrono::steady_clock::now();
      volatile double sink = work();
      (void)sink;
      sync.arrive_and_wait();
      double secs = std::chrono::duration<double>(
                        std::chrono::steady_clock::now() - t0)
                        .count();
      round_rates.push_back(static_cast<double>(threads) * per_round / secs);
    }
    stop.store(true, std::memory_order_release);
    sync.arrive_and_wait();
    for (auto &t : pool) t.join();

    std::sort(round_rates.begin(), round_rates.end());
    double median = round_rates[round_rates.size() / 2];
    if (base == 0.0) base = median;
    std::printf("%-8d %14.2f %9.2fx\n", threads, median / 1e6, median / base);
    (void)go;
  }
  return 0;
}
