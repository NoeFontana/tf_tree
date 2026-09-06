// Both engines in one C++ process, with no Rust binding on either arm.
//
// # What this measures, and why it is the fair version
//
// `crates/tf_tree_bench/src/ratio.rs` measures the same quotient from Rust, and
// its tf2 arm goes through `tf_tree_tf2_sys`. `docs/benchmarks/tf2.md` prices
// that boundary at **45.3 ns / 10% at depth 3** (bias 3: cross-TU, no inlining,
// one extra copy) — 498.2 ns through the binding against 452.9 ns native, a
// subtraction between two rows of that document's bracket table, one of which
// **this file produces**. It is charged to tf2 — so every ratio measured that
// way flatters `tf_tree`. (This line read `~21 ns / 8%` until 2026-09-05;
// `tf2.md` withdrew that figure for having no derivation recorded anywhere and
// for disagreeing with its own bracket table by a factor of two. To find every
// site that still carries it, grep — an enumeration written here is a list that
// goes stale silently, which is how this file came to cite the document that
// withdrew the number it was quoting.)
//
// Here the arms are the other way round:
//
//   * **tf2 is native.** `tf2::BufferCore::lookupTransform` called directly from
//     C++, the call a real node makes. It pays nothing.
//   * **tf_tree goes through its C ABI**, linked as a shared library from C++.
//
// So the residual cost works against the claim rather than for it, and this
// number is a lower bound rather than a flattering upper one. That is the
// direction a published ratio should err.
//
// **How much it is charged was a surprise, and this comment used to state it
// wrongly.** It said ~2%, on the strength of `docs/PHASE4.md` §7 gate 1's
// `tft_plan_at` = 1.020x native Rust. Measured here, on the same host and
// fixture as the Rust harness: **306.7 ns against 201.5 ns, or +52%.** §7 gate 1
// is `examples/abi_cost.rs`, which calls the ABI from *Rust inside the same
// build*, where the linker can still see across the call. A C++ caller against
// `libtf_tree_c.so` cannot, and this is what that costs.
//
// Two differences separate the two figures and **this run does not tell them
// apart**: the cross-`.so` call itself, and the fact that the arena here is a
// shared `memfd` mapping rather than a heap one.
//
// **They have since been told apart, elsewhere, and the answer is neither.**
// `just abi-split` (`crates/tf_tree_bench/src/backing.rs`) walks the ladder on
// the arena `native_arena` serves: the shared mapping costs <= 9.6 ns, attaching
// read-only from another process costs -0.7 ns, and the link mode costs ~1 ns
// (`tests/cpp/bench.cpp` compiled against the `.a` and the `.so` measures 245.4
// against 244.4). What remains is **+99.5 ns / +49% in the C ABI itself**:
// `tft_plan_at` constructs a `Guard` on every call where the Rust arm hoists
// one, and `tft_plan_at_many` recovers 41 ns by paying it once per batch.
//
// **A first version of this comment blamed the shared-library boundary.** It
// had reached that by subtracting a measured mapping cost from the total and
// attributing the residue; nothing had measured the boundary. It is 0.4%.
//
// The consequence for the reader: **neither this ratio nor the Rust one is
// "the" answer.** They bracket it. See `docs/benchmarks/tf2.md`.
//
// # Why both arms are still in one process
//
// Because the pairing is what makes the number resolvable at all. The arms are
// interleaved within every round and the leading arm alternates, so drift common
// to both divides out of each round's quotient; the Rust harness reports a ~3%
// band that way on a host whose absolute latencies are `unavailable`. Two
// separate binaries cannot be interleaved, and comparing their medians puts this
// host's ~4% run-to-run spread straight into the answer — which is the failure
// `cpp-bench`'s §7 gate 2 had before it started interleaving.
//
// # Why an arena is attached rather than built
//
// `tft_tree_open` attaches; it cannot create. That is D18 — a consumer linked
// against the C ABI joins read-only and the MMU enforces it — and it is not
// something to work around for a benchmark. `native_arena` is the Rust owner
// that serves the arena and dumps the identical `.tfstream` this program feeds
// to tf2, so both engines hold the same data by construction.

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

// The same parser `native_scaling.cpp` uses, on the same format.
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

// A real broadcaster stores its authority once. Constructing it per call was
// bias 4 in `tf2.md` — 20 characters is past libstdc++'s 15-byte SSO buffer, so
// a literal costs one heap allocation on every `setTransform`, charged to tf2.
const std::string kAuthority = "tf_tree_native_ratio";

// `tft_last_error` fills a caller-owned struct rather than returning a pointer —
// the ABI holds no global string for a caller to outlive.
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

// max(rotation-angle error in rad, translation error in m) — the same metric the
// Rust differential scores with, so "agree" means the same thing on both sides.
//
// **The angle comes from the chord, not from `acos` of the dot product**, and
// the first version of this function got it wrong in a way worth recording. Near
// identity — which is where two engines agreeing spend all their time — `acos`
// is catastrophically ill-conditioned: at `w = 1 - 1e-15` it returns ~4.5e-8,
// so the *metric* manufactures a disagreement of about 2^-24 out of two poses
// that are equal to the last bit. That is exactly the cancellation
// `tf_tree_math`'s `interp.rs` refuses to write, for the same reason, and the
// check below duly refused to time anything until it was fixed.
//
// For unit quaternions the rotation angle between them satisfies
// `sin(theta/2) = |qa - qb| / 2`, and `asin` near zero is well conditioned.
// `q` and `-q` are the same rotation, so the shorter of the two chords wins.
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

  // `atoi` maps anything unparseable to 0, and the script forwards "$@"
  // straight here. `rounds <= 0` leaves `ratios` empty and `min_element` then
  // dereferences `end()`; `sweeps <= 0` makes `per_round` zero and every timing
  // divide by it. Both are refusals, not clamps: a run that silently measured
  // something other than what was asked for is the failure this whole file is
  // written to avoid.
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

  // The stamp sweep, off every dynamic grid so the interpolator actually runs.
  // Same construction as the Rust harness and for the same reason: `NOW_NS` is a
  // knot on all four rates, and a sweep anchored there measures `bracket` plus a
  // seqlock read rather than interpolation. That is `docs/decisions/0013`.
  const std::int64_t kNowNs = 9900000000LL;
  std::vector<std::int64_t> stamps;
  stamps.reserve(256);
  for (std::int64_t i = 0; i < 256; ++i) {
    stamps.push_back(kNowNs - 3700000LL - i * 9631LL);
  }

  // ---- agreement, before anything is timed -------------------------------
  //
  // An arm that is fast because it is answering a different question would move
  // the ratio and nothing in the timing would say so.
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

  // Warm both arms: tf2 walks the topology per call and fills its own caches,
  // and ours faults in the rings through a fresh mapping.
  volatile double sink = 0.0;
  for (int i = 0; i < 20; ++i) { sink += sweep_ours(); sink += sweep_theirs(); }

  const double per_round = static_cast<double>(sweeps) * static_cast<double>(stamps.size());
  std::vector<double> ratios, ours_ns, theirs_ns;
  for (int r = 0; r < rounds; ++r) {
    double a = 0.0, b = 0.0;
    // Alternate the leading arm: a fixed order gives one arm the colder cache in
    // every round, which the pairing would preserve rather than cancel.
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
