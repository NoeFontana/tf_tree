// Both engines in one C++ process, with no Rust binding on either arm.
//
// `crates/tf_tree_bench/src/ratio.rs` measures the same quotient with tf2 behind
// `tf_tree_tf2_sys`. Here tf2 is native and tf_tree goes through its C ABI, so the ratio is a
// lower bound; the two bracket the answer (`docs/benchmarks/tf2.md`, `just abi-split`).
//
// Arms are interleaved within every round and the leading arm alternates, so common drift
// divides out. `tft_tree_open` attaches and cannot create (D18): `native_arena` serves the
// arena and dumps the `.tfstream` fed to tf2, so both engines hold the same data.


#include <tf2/buffer_core.hpp>
#include <geometry_msgs/msg/transform_stamped.hpp>

extern "C" {
#include "tf_tree.h"
}

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <fstream>
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

// The same parser `native_scaling.cpp` uses.
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

// Stored once: a literal past the 15-byte SSO limit would allocate per call.
const std::string kAuthority = "tf_tree_native_ratio";

// `tft_last_error` fills a caller-owned struct.
const char *last_error() {
  static tft_error e;
  if (tft_last_error(&e) != TFT_OK) return "(no error recorded)";
  return e.message;
}

double median(std::vector<double> v) {
  if (v.empty()) return std::nan("");
  std::sort(v.begin(), v.end());
  return v[v.size() / 2];
}

// max(rotation-angle error in rad, translation error in m), as the Rust differential scores.
// The angle comes from the chord, `sin(theta/2) = |qa - qb| / 2`, not `acos` (ill-conditioned
// near identity); the shorter of the `q`/`-q` chords wins.
double pose_error(const double *qa, const double *ta,
                  const tf2::Quaternion &qb, const tf2::Vector3 &tb) {
  const double b[4] = {qb.w(), qb.x(), qb.y(), qb.z()};
  double diff = 0.0, sum = 0.0;
  for (int i = 0; i < 4; ++i) {
    const double d = qa[i] - b[i], s2 = qa[i] + b[i];
    diff += d * d;
    sum += s2 * s2;
  }
  const double chord = std::sqrt(std::min(diff, sum));
  const double rot = 2.0 * std::asin(std::min(1.0, chord / 2.0));
  const double dx = ta[0] - tb.x(), dy = ta[1] - tb.y(), dz = ta[2] - tb.z();
  return std::max(rot, std::sqrt(dx * dx + dy * dy + dz * dz));
}

}  // namespace

int main(int argc, char **argv) {
  const std::string stream_path =
      argc > 1 ? argv[1] : "target/native/fixture.tfstream";
  const char *target = argc > 2 ? argv[2] : "imu_link";
  const char *source = argc > 3 ? argv[3] : "map";
  const int rounds = argc > 4 ? std::atoi(argv[4]) : 9;
  const int sweeps = argc > 5 ? std::atoi(argv[5]) : 40;

  // `atoi` maps garbage to 0: refuse `rounds <= 0` or `sweeps <= 0`, don't clamp.
  if (rounds <= 0 || sweeps <= 0) {
    std::fprintf(stderr,
                 "rounds and sweeps must both be positive (got %d and %d); "
                 "non-numeric arguments parse as 0\n",
                 rounds, sweeps);
    return 2;
  }

  if (tft_check_abi(TFT_ABI_VERSION_MAJOR, TFT_ABI_VERSION_MINOR) != TFT_OK) {
    std::fprintf(stderr, "tft_check_abi failed: header and library disagree\n");
    return 1;
  }

  // ---- tf2, native -------------------------------------------------------
  const Stream s = load(stream_path);
  tf2::BufferCore buf(tf2::durationFromSec(30.0));
  for (const auto &x : s.statics) buf.setTransform(to_msg(x), kAuthority, true);
  for (const auto &x : s.dynamics) buf.setTransform(to_msg(x), kAuthority, false);

  // ---- tf_tree, through the C ABI ---------------------------------------
  tft_tree *tree = nullptr;
  if (tft_tree_open(&tree) != TFT_OK) {
    std::fprintf(stderr,
                 "tft_tree_open failed: %s\n"
                 "Is `native_arena` running, and are TF_TREE_NAME / "
                 "TF_TREE_RUNTIME_DIR set to the arena it serves?\n",
                 last_error());
    return 1;
  }
  tft_plan *plan = nullptr;
  if (tft_plan_create(tree, target, source, &plan) != TFT_OK) {
    std::fprintf(stderr, "tft_plan_create(%s <- %s) failed: %s\n", source, target,
                 last_error());
    tft_tree_free(tree);
    return 1;
  }

  // The stamp sweep, off every dynamic grid so the interpolator runs
  // (`docs/decisions/0013`).
  const std::int64_t kNowNs = 9900000000LL;
  std::vector<std::int64_t> stamps;
  stamps.reserve(256);
  for (std::int64_t i = 0; i < 256; ++i) {
    stamps.push_back(kNowNs - 3700000LL - i * 9631LL);
  }

  // ---- agreement, before anything is timed -------------------------------
  std::size_t agreed = 0;
  double worst = 0.0;
  for (std::int64_t ns : stamps) {
    double out[7];
    if (tft_plan_at(plan, ns, TFT_LAYOUT_QVEC7_WXYZ, out) != TFT_OK) {
      std::fprintf(stderr, "tf_tree declined stamp %lld: %s\n",
                   static_cast<long long>(ns), last_error());
      return 1;
    }
    geometry_msgs::msg::TransformStamped m;
    try {
      m = buf.lookupTransform(target, source, tf2::TimePoint(std::chrono::nanoseconds(ns)));
    } catch (const tf2::TransformException &e) {
      std::fprintf(stderr, "tf2 declined stamp %lld: %s\n",
                   static_cast<long long>(ns), e.what());
      return 1;
    }
    const tf2::Quaternion q(m.transform.rotation.x, m.transform.rotation.y,
                            m.transform.rotation.z, m.transform.rotation.w);
    const tf2::Vector3 t(m.transform.translation.x, m.transform.translation.y,
                         m.transform.translation.z);
    const double d = pose_error(out, out + 4, q, t);
    worst = std::max(worst, d);
    if (d > 1e-9) {
      std::fprintf(stderr,
                   "the two engines disagree at stamp %lld by %g; a ratio between "
                   "arms answering different questions is not a measurement\n",
                   static_cast<long long>(ns), d);
      return 1;
    }
    ++agreed;
  }

  auto sweep_ours = [&]() {
    double acc = 0.0;
    double out[7];
    for (int k = 0; k < sweeps; ++k) {
      for (std::int64_t ns : stamps) {
        if (tft_plan_at(plan, ns, TFT_LAYOUT_QVEC7_WXYZ, out) == TFT_OK) acc += out[4];
      }
    }
    return acc;
  };
  auto sweep_theirs = [&]() {
    double acc = 0.0;
    for (int k = 0; k < sweeps; ++k) {
      for (std::int64_t ns : stamps) {
        const auto m = buf.lookupTransform(target, source,
                                           tf2::TimePoint(std::chrono::nanoseconds(ns)));
        acc += m.transform.translation.x;
      }
    }
    return acc;
  };

  // Warm both arms.
  volatile double sink = 0.0;
  for (int i = 0; i < 20; ++i) { sink += sweep_ours(); sink += sweep_theirs(); }

  const double per_round = static_cast<double>(sweeps) * static_cast<double>(stamps.size());
  std::vector<double> ratios, ours_ns, theirs_ns;
  for (int r = 0; r < rounds; ++r) {
    double a = 0.0, b = 0.0;
    // Alternate the leading arm so neither always gets the colder cache.
    if (r % 2 == 0) {
      auto t0 = std::chrono::steady_clock::now();
      sink += sweep_ours();
      a = std::chrono::duration<double, std::nano>(std::chrono::steady_clock::now() - t0).count() / per_round;
      auto t1 = std::chrono::steady_clock::now();
      sink += sweep_theirs();
      b = std::chrono::duration<double, std::nano>(std::chrono::steady_clock::now() - t1).count() / per_round;
    } else {
      auto t1 = std::chrono::steady_clock::now();
      sink += sweep_theirs();
      b = std::chrono::duration<double, std::nano>(std::chrono::steady_clock::now() - t1).count() / per_round;
      auto t0 = std::chrono::steady_clock::now();
      sink += sweep_ours();
      a = std::chrono::duration<double, std::nano>(std::chrono::steady_clock::now() - t0).count() / per_round;
    }
    if (a <= 0.0) {
      std::fprintf(stderr, "a timed round measured %g ns per lookup\n", a);
      return 1;
    }
    ratios.push_back(b / a);
    ours_ns.push_back(a);
    theirs_ns.push_back(b);
  }

  const double lo = *std::min_element(ratios.begin(), ratios.end());
  const double hi = *std::max_element(ratios.begin(), ratios.end());

  // Machine-readable, one `key value` per line, for the Rust side to parse.
  std::printf("schema tf_tree.native-ratio/1\n");
  std::printf("speedup_vs_tf2 %.6f\n", median(ratios));
  std::printf("ratio_lo %.6f\n", lo);
  std::printf("ratio_hi %.6f\n", hi);
  std::printf("tf_tree_ns %.4f\n", median(ours_ns));
  std::printf("tf2_ns %.4f\n", median(theirs_ns));
  std::printf("rounds %d\n", rounds);
  std::printf("lookups_per_round %.0f\n", per_round);
  std::printf("agreed %zu\n", agreed);
  std::printf("max_deviation %.3e\n", worst);

  tft_plan_free(plan);
  tft_tree_free(tree);
  return 0;
}
