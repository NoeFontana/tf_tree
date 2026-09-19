// One consumer process, either engine, over a real DDS (`docs/PHASE5.md` §9.1).
//
// Modes:
// * `--mode tf2` — `tf2_ros::Buffer` + `TransformListener`, `--consumers N` query
//   threads. Run as N processes of 1 (ordinary deployment) or one process of N
//   (tf2's best case, the control).
// * `--mode tf_tree` — §5.8 form 3: hosts the ingest bridge and `--consumers N`
//   threads read the arena it fills.
// * `--mode tf_tree_bridge` + `--mode tf_tree_attach` — §9.1's "one bridge plus N
//   consumers" as N+1 processes. The bridge hosts form 3 under a non-empty
//   `arena_name` with no query threads; each attach process joins read-only via
//   `tft_tree_open()`, subscribing to nothing.
//
// The bridge emits the same stats block as every process with `consumers 0`;
// `dds_report` sums `cpu_ns` and `pss_kib` per arm label over the summed consumer
// count, so its cost lands in its arm. It is a mode rather than `--consumers 0`
// because `--consumers 0` stays refused (an empty query arm would look perfect).
//
// Where tf_tree is worse (§9.3): `tf_tree.processes` runs N+1 processes to tf2's
// N, plus a `memfd`, a rendezvous entry and a participant slot.
//
// Measurement is `measure.hpp`: open-loop schedule, `service` and `cycle`
// distributions, whole-process CPU in ns, PSS.

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
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

#if !defined(TFT_HAVE_SHM)
// `#error`, not a runtime refusal (as in `test_shared_arena.cpp`): otherwise the
// `tf_tree.processes` arm would silently stop being built.
#error "TFT_HAVE_SHM is not defined: libtf_tree_c was built without --features shm, \
or the CMake package's nm probe did not find tft_tree_open in it. See ros/build.sh step 1."
#endif

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
  /// How far behind "now" every query is aimed, in ns; a query at the present
  /// asks for a sample that has not arrived (an error path).
  int64_t lag_ns = 100'000'000;
  /// `--mode tf_tree_bridge` only: keep serving this long after the measured
  /// window, since consumers start late and would otherwise read an unwritten
  /// arena. The driver sets it.
  double linger = 0.0;
  /// `--mode tf_tree_attach` only: how long to wait for the arena to appear.
  double attach_timeout = 30.0;
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
    // Warm-up samples are discarded (§9.3).
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
  // The 10 s default cache, as deployed and as the tf_tree rings retain.
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
  printf("cpu_ns %lu\n", after.cpu_since(before));
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
    // Same stamp policy as the tf2 arm.
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

/// Compile one plan per query pair; frees what it compiled and empties `out` on
/// failure.
bool compile_plans(
  tft_tree * tree, const std::vector<Pair> & pairs, std::vector<tft_plan *> & out)
{
  for (const auto & p : pairs) {
    tft_plan * plan = nullptr;
    const tft_status s = tft_plan_create(tree, p.target.c_str(), p.source.c_str(), &plan);
    if (s != TFT_OK) {
      fprintf(
        stderr, "bench_consumer: cannot plan %s <- %s (status %d)\n",
        p.target.c_str(), p.source.c_str(), static_cast<int>(s));
      for (auto * done : out) {tft_plan_free(done);}
      out.clear();
      return false;
    }
    out.push_back(plan);
  }
  return true;
}

/// Spin the node, warm up, measure `--consumers N` query threads over `plans`,
/// and emit the per-thread and per-process blocks. Shared by `--mode tf_tree`
/// and `tf_tree_attach` so the arms differ only in where the arena came from.
void measure_tf_tree_consumers(
  const std::vector<tft_plan *> & plans, const std::shared_ptr<rclcpp::Node> & node,
  const Args & args)
{
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

  for (size_t i = 0; i < results.size(); ++i) {print_result(i, results[i]);}
  printf("cpu_ns %lu\n", after.cpu_since(before));
  printf("pss_kib %lu\n", after.pss_kib);
}

/// The bridge's own account of what it ingested; `dds_report::check_structure`
/// refuses an arm whose bridge received zero transforms.
void print_bridge_stats(const tf_tree_ros::BridgeHandle & bridge)
{
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
}

int run_tf_tree(const Args & args, const std::vector<Pair> & pairs)
{
  auto node = std::make_shared<rclcpp::Node>("tf_bench_tf_tree_consumer");

  tf_tree_ros::BridgeOptions o;
  o.topology_toml = read_file(args.topology_path);
  // Defaults, including `first_writer_wins`: a bridge tuned for the benchmark
  // would not be the bridge an operator deploys (§9.3).
  tf_tree_ros::BridgeHandle bridge(node.get(), o);

  std::vector<tft_plan *> plans;
  if (!compile_plans(bridge.tree(), pairs, plans)) {return 1;}

  measure_tf_tree_consumers(plans, node, args);

  for (auto * p : plans) {tft_plan_free(p);}
  print_bridge_stats(bridge);
  return 0;
}

// ---------------------------------------------------------------------------
// tf_tree, across processes — `docs/decisions/0015` step 5
// ---------------------------------------------------------------------------

/// The rendezvous name both halves select by: `$TF_TREE_NAME`, not a flag, since
/// `tft_tree_open()` takes no name and the environment selects the arena.
std::string arena_name_from_env()
{
  const char * n = std::getenv("TF_TREE_NAME");
  return n == nullptr ? std::string() : std::string(n);
}

/// `tft_tree_open` until it succeeds or `timeout` passes; nullptr on timeout.
///
/// The C ABI has no timeout parameter, so poll (as `test_shared_arena.cpp`'s
/// `open_within` does).
tft_tree * open_within(std::chrono::duration<double> timeout)
{
  const auto deadline = std::chrono::steady_clock::now() + timeout;
  for (;;) {
    tft_tree * tree = nullptr;
    if (tft_tree_open(&tree) == TFT_OK) {
      return tree;
    }
    if (std::chrono::steady_clock::now() >= deadline) {
      return nullptr;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
  }
}

/// One bridge process: hosts §5.8 form 3 under a rendezvous name and serves the
/// arm's consumers; reports `consumers 0` (see the file header).
int run_tf_tree_bridge(const Args & args)
{
  const std::string name = arena_name_from_env();
  if (name.empty()) {
    fprintf(
      stderr,
      "bench_consumer: --mode tf_tree_bridge needs $TF_TREE_NAME set to the rendezvous name\n"
      "                the arm's --mode tf_tree_attach processes will open. See ros/dds_bench.sh.\n");
    return 2;
  }

  auto node = std::make_shared<rclcpp::Node>("tf_bench_tf_tree_bridge");

  tf_tree_ros::BridgeOptions o;
  o.topology_toml = read_file(args.topology_path);
  // Defaults everywhere else (see `run_tf_tree`).
  o.arena_name = name;
  // A `BridgeError` here has no heap fallback (`docs/decisions/0015`); it exits
  // non-zero and `dds_bench.sh` stops the run.
  tf_tree_ros::BridgeHandle bridge(node.get(), o);

  std::thread spinner([&node]() {rclcpp::spin(node);});

  // Same warm-up and window as the consumers it serves.
  std::this_thread::sleep_for(std::chrono::duration<double>(args.warmup));
  const auto before = ProcStats::read();
  std::this_thread::sleep_for(std::chrono::duration<double>(args.seconds));
  const auto after = ProcStats::read();
  // Outside the measured window on purpose — see `Args::linger`.
  std::this_thread::sleep_for(std::chrono::duration<double>(args.linger));

  rclcpp::shutdown();
  spinner.join();

  printf("cpu_ns %lu\n", after.cpu_since(before));
  printf("pss_kib %lu\n", after.pss_kib);
  print_bridge_stats(bridge);
  return 0;
}

/// One consumer process: attaches read-only to the arena a `tf_tree_bridge`
/// process published and runs `--consumers N` query threads. It hosts no bridge
/// and no `/tf` subscription (the bridge pays that once; `dds_report`'s footer
/// says so), but does spin an rclcpp node: queries are aimed with `node->now()`
/// like every arm, and PSS/CPU stay comparable with a DDS participant present.
int run_tf_tree_attach(const Args & args, const std::vector<Pair> & pairs)
{
  const std::string name = arena_name_from_env();
  if (name.empty()) {
    fprintf(
      stderr,
      "bench_consumer: --mode tf_tree_attach needs $TF_TREE_NAME set to the rendezvous name\n"
      "                the arm's --mode tf_tree_bridge process publishes. See ros/dds_bench.sh.\n");
    return 2;
  }

  auto node = std::make_shared<rclcpp::Node>("tf_bench_tf_tree_attach");

  tft_tree * tree = open_within(std::chrono::duration<double>(args.attach_timeout));
  if (tree == nullptr) {
    const char * domain = std::getenv("TF_TREE_DOMAIN");
    const char * dir = std::getenv("TF_TREE_RUNTIME_DIR");
    fprintf(
      stderr,
      "bench_consumer: no arena named \"%s\" appeared within %.1fs, so there is nothing to\n"
      "                attach to: $TF_TREE_DOMAIN=%s $TF_TREE_RUNTIME_DIR=%s.\n"
      "                The arm's --mode tf_tree_bridge process either did not start or\n"
      "                published under different coordinates; its .err file says which.\n",
      name.c_str(), args.attach_timeout, domain == nullptr ? "<unset>" : domain,
      dir == nullptr ? "<unset>" : dir);
    return 1;
  }

  std::vector<tft_plan *> plans;
  if (!compile_plans(tree, pairs, plans)) {
    tft_tree_free(tree);
    return 1;
  }

  measure_tf_tree_consumers(plans, node, args);

  for (auto * p : plans) {tft_plan_free(p);}
  tft_tree_free(tree);
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
    } else if (a == "--linger" && i + 1 < argc) {
      args.linger = std::stod(next());
    } else if (a == "--attach-timeout" && i + 1 < argc) {
      args.attach_timeout = std::stod(next());
    }
  }

  const bool is_bridge = args.mode == "tf_tree_bridge";
  // The one process that is not a consumer; a mode so `--consumers 0` stays refused.
  if (is_bridge) {args.consumers = 0;}

  const bool known_mode = args.mode == "tf2" || args.mode == "tf_tree" ||
    args.mode == "tf_tree_attach" || is_bridge;
  if (!known_mode || args.queries_path.empty() || (args.consumers == 0 && !is_bridge)) {
    fprintf(
      stderr,
      "usage: bench_consumer --mode tf2|tf_tree|tf_tree_bridge|tf_tree_attach\n"
      "                      --queries <file> [--topology <file>]\n"
      "                      [--consumers N] [--hz H] [--seconds S] [--warmup W]\n"
      "                      [--linger S] [--attach-timeout S]\n"
      "\n"
      "  tf_tree_bridge and tf_tree_attach are the two halves of one arm and\n"
      "  select the same arena through $TF_TREE_NAME; see ros/dds_bench.sh.\n");
    return 2;
  }
  if ((args.mode == "tf_tree" || is_bridge) && args.topology_path.empty()) {
    fprintf(stderr, "bench_consumer: --mode %s needs --topology\n", args.mode.c_str());
    return 2;
  }

  try {
    const auto pairs = read_queries(args.queries_path);
    // §9.3: the discarded warm-up window is reported.
    printf("warmup_s %.1f\n", args.warmup);
    printf("measured_s %.1f\n", args.seconds);
    printf("consumers %zu\n", args.consumers);
    if (args.mode == "tf2") {return run_tf2(args, pairs);}
    if (args.mode == "tf_tree") {return run_tf_tree(args, pairs);}
    if (is_bridge) {return run_tf_tree_bridge(args);}
    return run_tf_tree_attach(args, pairs);
  } catch (const std::exception & e) {
    fprintf(stderr, "bench_consumer: %s\n", e.what());
    return 1;
  }
}
