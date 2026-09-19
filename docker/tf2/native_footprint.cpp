// What `tf2::BufferCore` costs in memory, in a native C++ process of its own: the
// tf2 half of the comparison whose tf_tree half is `footprint`'s `mem-tf_tree`
// mode. Two processes, not two modes, so neither engine's freed chunks satisfy
// the other's requests and no Rust runtime, allocator or shim is weighed.
//
// Two instruments:
//   * `mallinfo2`'s `uordblks + hblkhd`: glibc's accounting, identical for both
//     engines. `hblkhd` is required: the arena is one allocation above the mmap
//     threshold.
//   * Pss from `/proc/self/smaps_rollup`: what `top` shows; it sees address space
//     an allocator holds unfaulted (decision `0021`). The reader mirrors
//     `ros/tf_tree_bench_ros/include/tf_tree_bench_ros/measure.hpp`.
//
// The fixture is the `.tfstream` from `native_arena --dump-only`. The cache is
// `HISTORY_SECS * 3.0 = 30 s`, matching `tf2.rs`'s `CACHE_SECS`, so nothing is
// evicted.

#include <tf2/buffer_core.hpp>
#include <geometry_msgs/msg/transform_stamped.hpp>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <malloc.h>
#include <sstream>
#include <string>
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

// The same parser `native_ratio.cpp` and `native_scaling.cpp` use.
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

// Stored once: a literal past the 15-byte SSO limit would allocate per call, into
// the very counter this program reads.
const std::string kAuthority = "tf_tree_native_ratio";

// In-use bytes across the sbrk heap and mmapped regions; mirrors `footprint.rs`.
std::size_t heap_in_use() {
  const struct mallinfo2 mi = ::mallinfo2();
  return mi.uordblks + mi.hblkhd;
}

// Pss in KiB, or 0 if `/proc/self/smaps_rollup` is unreadable.
std::uint64_t self_pss_kib() {
  std::ifstream smaps("/proc/self/smaps_rollup");
  std::string line;
  while (std::getline(smaps, line)) {
    if (line.rfind("Pss:", 0) == 0) {
      std::istringstream f(line.substr(4));
      std::uint64_t kib = 0;
      f >> kib;
      return kib;
    }
  }
  return 0;
}

}  // namespace

int main(int argc, char **argv) {
  const std::string path = argc > 1 ? argv[1] : "target/native/fixture.tfstream";

  // Parse before the first measurement: the input is the harness's memory, not
  // tf2's.
  const Stream s = load(path);

  // Warm the allocator so first-use bookkeeping is not charged to the engine.
  { std::string warm(1024, 'x'); (void)warm.size(); }

  const std::size_t heap_before = heap_in_use();
  const std::uint64_t pss_before = self_pss_kib();

  tf2::BufferCore buf(tf2::durationFromSec(30.0));
  for (const auto &x : s.statics) buf.setTransform(to_msg(x), kAuthority, true);
  for (const auto &x : s.dynamics) buf.setTransform(to_msg(x), kAuthority, false);

  const std::size_t heap_after = heap_in_use();
  const std::uint64_t pss_after = self_pss_kib();

  const std::size_t stored = s.dynamics.size();
  const std::size_t heap = heap_after - heap_before;

  std::printf("engine\ttf2\n");
  std::printf("heap_bytes\t%zu\n", heap);
  std::printf("pss_kib_delta\t%llu\n",
              static_cast<unsigned long long>(pss_after - pss_before));
  std::printf("pss_kib_total\t%llu\n", static_cast<unsigned long long>(pss_after));
  std::printf("samples_stored\t%zu\n", stored);
  std::printf("static_edges\t%zu\n", s.statics.size());
  // tf2 declares no slots; `n/a` rather than omitted so the tables diff cleanly.
  std::printf("declared_slots\tn/a\n");
  std::printf("bytes_per_slot\tn/a\n");
  std::printf("bytes_per_sample\t%.1f\n",
              stored ? static_cast<double>(heap) / static_cast<double>(stored) : 0.0);

  // Keep the buffer alive past the second reading.
  if (buf.allFramesAsString().empty()) std::fprintf(stderr, "empty tf2 buffer\n");
  return 0;
}
