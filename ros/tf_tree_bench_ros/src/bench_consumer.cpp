// One consumer process, either engine, over a real DDS.
//
// This is the half of `docs/PHASE5.md` §9.1 that no other harness in the
// repository can reach. `mp_bench`'s tf2 column says so in its own output:
//
//   "this tf2 column is a FLOOR. Each consumer holds a private BufferCore built
//    from the identical stream, so it shows the memory and CPU duplication that
//    having no shared arena forces — but no transport. A deployed tf2 consumer
//    reaches the tree only through a TransformListener over DDS and additionally
//    pays deserialization and fan-out."
//
// Here it pays it.
//
// # Modes, and the arm each one composes
//
// * `--mode tf2` — a `tf2_ros::Buffer` fed by a `TransformListener` over DDS,
//   with `--consumers N` query threads sharing it. The driver runs this two
//   ways, and they are different experiments:
//     - N processes of `--consumers 1`: the ordinary ROS deployment, one
//       listener per node, which is what the memory and CPU claims are about.
//     - one process of `--consumers N`: a composed container, tf2's **best
//       case**, one listener shared by N threads. It is here so the comparison
//       has a control and cannot be read as a strawman.
// * `--mode tf_tree` — §5.8 form 3: this process hosts the ingest bridge on its
//   own node and `--consumers N` threads read the arena it fills.
//
// # What is NOT here, and why — read before comparing arms
//
// **There is no multi-process tf_tree arm**, and its absence is a fact about the
// engine today rather than an omission. `tft_bridge_create` builds its arena
// with `TreeBuilder::build()` — a *heap* arena — so no second process can attach
// to what the bridge fills. Giving the bridge a shared arena is new public API
// on the C ABI's §5 surface, which `CLAUDE.md` routes to a decision record, not
// to a benchmark. Until that record exists, the honest framing of the
// comparison is exactly what §9.3 prescribes: report the arms that can be
// measured fairly, and say plainly which one cannot and why.
//
// # Measurement
//
// `measure.hpp`, which mirrors `crates/tf_tree_bench/src/mp.rs`: open-loop
// schedule (a stall must show up as latency, not as fewer samples), a `service`
// distribution for the engine's own cost and a `cycle` distribution for what the
// node experiences, CPU from `schedstat` in nanoseconds, and PSS rather than
// summed RSS.

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <fstream>
#include <memory>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

#include "rclcpp/rclcpp.hpp"
#include "tf2_ros/buffer.hpp"
#include "tf2_ros/transform_listener.hpp"

#include "tf_tree_bench_ros/measure.hpp"
#include "tf_tree_ros/bridge_handle.hpp"

extern "C" {
#include "tf_tree.h"
}

namespace
{

using tf_tree_bench_ros::Histogram;
using tf_tree_bench_ros::ProcStats;
using tf_tree_bench_ros::RateLoop;

struct Args
{
  std::string mode = "tf2";
  std::string queries_path;
  std::string topology_path;
  size_t consumers = 1;
  double hz = 100.0;
  double seconds = 20.0;
  double warmup = 3.0;
  /// How far behind "now" every query is aimed, in nanoseconds.
  ///
  /// 100 ms, and both arms use it. A query aimed at the present asks for a
  /// sample that has not arrived, which is an error path — and timing an error
  /// path is timing an error path, not an engine. `mp_compare.py` carries the
  /// same constant for the same reason.
  int64_t lag_ns = 100'000'000;
};

struct Pair
{
  std::string target;
  std::string source;
};

std::vector<Pair> read_queries(const std::string & path)
{
  std::ifstream f(path);
  if (!f) {throw std::runtime_error("cannot read queries file " + path);}
  std::vector<Pair> out;
  std::string line;
  while (std::getline(f, line)) {
    if (line.empty() || line[0] == '#') {continue;}
    std::istringstream s(line);
    Pair p;
    s >> p.target >> p.source;
    if (!p.target.empty() && !p.source.empty()) {out.push_back(p);}
  }
  if (out.empty()) {throw std::runtime_error("queries file " + path + " has no pairs");}
  return out;
}

std::string read_file(const std::string & path)
{
  std::ifstream f(path);
  if (!f) {throw std::runtime_error("cannot read " + path);}
  std::ostringstream s;
  s << f.rdbuf();
  return s.str();
}

/// One consumer thread's result.
struct ThreadResult
{
  Histogram service;
  Histogram cycle;
  uint64_t ok = 0;
  uint64_t err = 0;
};

/// Emit a thread's result in the driver's line protocol.
void print_result(size_t index, const ThreadResult & r)
{
  printf("consumer %zu service %s\n", index, r.service.encode().c_str());
  printf("consumer %zu cycle %s\n", index, r.cycle.encode().c_str());
  printf("consumer %zu ok %lu err %lu\n", index, r.ok, r.err);
}

// ---------------------------------------------------------------------------
// tf2
// ---------------------------------------------------------------------------

ThreadResult tf2_consumer_loop(
  tf2_ros::Buffer * buffer, rclcpp::Node * node, const std::vector<Pair> & pairs,
  const Args & args, const std::atomic<bool> & measuring, const std::atomic<bool> & stop)
{
  ThreadResult r;
  RateLoop rate(args.hz);
  size_t k = 0;
  while (!stop.load(std::memory_order_relaxed)) {
    const auto due = rate.next_due();
    const auto t0 = std::chrono::steady_clock::now();
    const auto & p = pairs[k % pairs.size()];
    const auto stamp = node->now() - rclcpp::Duration(0, 0) -
      rclcpp::Duration(std::chrono::nanoseconds(args.lag_ns));
    bool ok = false;
    try {
      (void)buffer->lookupTransform(p.target, p.source, tf2_ros::fromRclcpp(stamp));
      ok = true;
    } catch (const tf2::TransformException &) {
      ok = false;
    }
    const auto done = std::chrono::steady_clock::now();
    // Warm-up samples are discarded, not merely down-weighted: §9.3 requires
    // both stacks warmed and the discarded window stated.
    if (measuring.load(std::memory_order_relaxed)) {
      r.service.record(
        static_cast<uint64_t>(
          std::chrono::duration_cast<std::chrono::nanoseconds>(done - t0).count()));
      r.cycle.record(
        static_cast<uint64_t>(
          std::chrono::duration_cast<std::chrono::nanoseconds>(done - due).count()));
      if (ok) {++r.ok;} else {++r.err;}
    }
    ++k;
  }
  return r;
}

int run_tf2(const Args & args, const std::vector<Pair> & pairs)
{
  auto node = std::make_shared<rclcpp::Node>("tf_bench_tf2_consumer");
  // The 10 s default cache. Deliberately not tuned: it is what a deployed node
  // uses, and it is the same span the tf_tree fixture's rings retain.
  auto buffer = std::make_unique<tf2_ros::Buffer>(node->get_clock(), tf2::durationFromSec(10.0));
  auto listener = std::make_shared<tf2_ros::TransformListener>(*buffer, node, true);

  std::atomic<bool> measuring{false};
  std::atomic<bool> stop{false};
  std::thread spinner([&node]() {rclcpp::spin(node);});

  std::vector<ThreadResult> results(args.consumers);
  std::vector<std::thread> threads;
  for (size_t i = 0; i < args.consumers; ++i) {
    threads.emplace_back(
      [&, i]() {
        results[i] = tf2_consumer_loop(buffer.get(), node.get(), pairs, args, measuring, stop);
      });
  }

  std::this_thread::sleep_for(std::chrono::duration<double>(args.warmup));
  const auto before = ProcStats::read();
  measuring.store(true);
  std::this_thread::sleep_for(std::chrono::duration<double>(args.seconds));
  stop.store(true);
  for (auto & t : threads) {t.join();}
  const auto after = ProcStats::read();

  rclcpp::shutdown();
  spinner.join();

  for (size_t i = 0; i < results.size(); ++i) {print_result(i, results[i]);}
  printf("cpu_ns %lu\n", after.cpu_ns - before.cpu_ns);
  printf("pss_kib %lu\n", after.pss_kib);
  return 0;
}

// ---------------------------------------------------------------------------
// tf_tree
// ---------------------------------------------------------------------------

ThreadResult tf_tree_consumer_loop(
  const std::vector<tft_plan *> & plans, rclcpp::Node * node, const Args & args,
  const std::atomic<bool> & measuring, const std::atomic<bool> & stop)
{
  ThreadResult r;
  RateLoop rate(args.hz);
  size_t k = 0;
  double out[7];
  while (!stop.load(std::memory_order_relaxed)) {
    const auto due = rate.next_due();
    const auto t0 = std::chrono::steady_clock::now();
    // The identical stamp policy as the tf2 arm: `node->now()` minus the same
    // lag, from the same clock. Anything else would make the two arms ask
    // different questions.
    const int64_t stamp = node->now().nanoseconds() - args.lag_ns;
    const tft_status s = tft_plan_at(plans[k % plans.size()], stamp,
        TFT_LAYOUT_QVEC7_WXYZ, out);
    const auto done = std::chrono::steady_clock::now();
    if (measuring.load(std::memory_order_relaxed)) {
      r.service.record(
        static_cast<uint64_t>(
          std::chrono::duration_cast<std::chrono::nanoseconds>(done - t0).count()));
      r.cycle.record(
        static_cast<uint64_t>(
          std::chrono::duration_cast<std::chrono::nanoseconds>(done - due).count()));
      if (s == TFT_OK) {++r.ok;} else {++r.err;}
    }
    ++k;
  }
  return r;
}

int run_tf_tree(const Args & args, const std::vector<Pair> & pairs)
{
  auto node = std::make_shared<rclcpp::Node>("tf_bench_tf_tree_consumer");

  tf_tree_ros::BridgeOptions o;
  o.topology_toml = read_file(args.topology_path);
  // **Defaults, including `first_writer_wins`.** An earlier revision of this
  // file set `last_writer_wins` to work around a real defect: authority was
  // keyed on the resolved node name rather than on the GID, so a publisher the
  // graph renamed from `_NODE_NAME_UNKNOWN_` to its real name became a second
  // publisher and `first_writer_wins` rejected it forever — 9 864 of 10 070
  // transforms dropped here, and 100 % of lookups failing.
  //
  // That is fixed in `tf_tree_bridge` (identity is the GID; the name is
  // presentation), so the benchmark runs the configuration an operator deploys.
  // Reverting it here is also what keeps the fix honest: if the defect came
  // back, this arm would report a `FAILING` row again.
  // §9.3's "identical executor configuration" cuts both ways, and a bridge
  // tuned for the benchmark would not be the bridge an operator deploys.
  tf_tree_ros::BridgeHandle bridge(node.get(), o);

  std::vector<tft_plan *> plans;
  for (const auto & p : pairs) {
    tft_plan * plan = nullptr;
    const tft_status s = tft_plan_create(bridge.tree(), p.target.c_str(), p.source.c_str(), &plan);
    if (s != TFT_OK) {
      fprintf(
        stderr, "bench_consumer: cannot plan %s <- %s (status %d)\n",
        p.target.c_str(), p.source.c_str(), static_cast<int>(s));
      return 1;
    }
    plans.push_back(plan);
  }

  std::atomic<bool> measuring{false};
  std::atomic<bool> stop{false};
  std::thread spinner([&node]() {rclcpp::spin(node);});

  std::vector<ThreadResult> results(args.consumers);
  std::vector<std::thread> threads;
  for (size_t i = 0; i < args.consumers; ++i) {
    threads.emplace_back(
      [&, i]() {
        results[i] = tf_tree_consumer_loop(plans, node.get(), args, measuring, stop);
      });
  }

  std::this_thread::sleep_for(std::chrono::duration<double>(args.warmup));
  const auto before = ProcStats::read();
  measuring.store(true);
  std::this_thread::sleep_for(std::chrono::duration<double>(args.seconds));
  stop.store(true);
  for (auto & t : threads) {t.join();}
  const auto after = ProcStats::read();

  rclcpp::shutdown();
  spinner.join();

  for (auto * p : plans) {tft_plan_free(p);}

  for (size_t i = 0; i < results.size(); ++i) {print_result(i, results[i]);}
  printf("cpu_ns %lu\n", after.cpu_ns - before.cpu_ns);
  printf("pss_kib %lu\n", after.pss_kib);
  // The bridge's own account of what it ingested. A run whose bridge dropped
  // everything would otherwise report beautiful latencies for an empty arena.
  const auto st = bridge.stats();
  printf("bridge_transforms %lu\n", static_cast<uint64_t>(st.transforms));
  printf("bridge_applied %lu\n", static_cast<uint64_t>(st.applied));
  printf(
    "bridge_dropped %lu\n",
    static_cast<uint64_t>(
      st.dropped_authority + st.dropped_non_monotonic + st.dropped_bad_name +
      st.dropped_kind_change + st.dropped_undeclared + st.dropped_bad_pose +
      st.rejected_by_arena + st.refused_after_halt));
  printf("bridge_queue_high_water %lu\n", static_cast<uint64_t>(st.queue_high_water));
  return 0;
}

}  // namespace

int main(int argc, char ** argv)
{
  rclcpp::init(argc, argv);

  Args args;
  for (int i = 1; i < argc; ++i) {
    const std::string a = argv[i];
    auto next = [&]() {return std::string(argv[++i]);};
    if (a == "--mode" && i + 1 < argc) {args.mode = next();} else if (a == "--queries" &&
      i + 1 < argc)
    {
      args.queries_path = next();
    } else if (a == "--topology" && i + 1 < argc) {
      args.topology_path = next();
    } else if (a == "--consumers" && i + 1 < argc) {
      args.consumers = std::stoul(next());
    } else if (a == "--hz" && i + 1 < argc) {
      args.hz = std::stod(next());
    } else if (a == "--seconds" && i + 1 < argc) {
      args.seconds = std::stod(next());
    } else if (a == "--warmup" && i + 1 < argc) {
      args.warmup = std::stod(next());
    }
  }
  if (args.queries_path.empty() || args.consumers == 0) {
    fprintf(
      stderr,
      "usage: bench_consumer --mode tf2|tf_tree --queries <file> [--topology <file>]\n"
      "                      [--consumers N] [--hz H] [--seconds S] [--warmup W]\n");
    return 2;
  }
  if (args.mode == "tf_tree" && args.topology_path.empty()) {
    fprintf(stderr, "bench_consumer: --mode tf_tree needs --topology\n");
    return 2;
  }

  try {
    const auto pairs = read_queries(args.queries_path);
    // Stated in the output, because §9.3 requires the discarded warm-up window
    // to be reported rather than merely applied.
    printf("warmup_s %.1f\n", args.warmup);
    printf("measured_s %.1f\n", args.seconds);
    printf("consumers %zu\n", args.consumers);
    return args.mode == "tf2" ? run_tf2(args, pairs) : run_tf_tree(args, pairs);
  } catch (const std::exception & e) {
    fprintf(stderr, "bench_consumer: %s\n", e.what());
    return 1;
  }
}
