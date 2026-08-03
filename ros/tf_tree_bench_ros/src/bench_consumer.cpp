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
//   own node and `--consumers N` threads read the arena it fills. The `tf2`
//   composed arm's counterpart.
// * `--mode tf_tree_bridge` + `--mode tf_tree_attach` — the two halves of
//   §9.1's actual sentence, *"one bridge plus N `tf_tree` consumers"*, as N+1
//   processes. The bridge hosts form 3 with a non-empty `arena_name` and runs
//   **no** query threads; each attach process joins that arena read-only with
//   `tft_tree_open()` and runs `--consumers N` query threads, hosting no bridge
//   and no subscription to `/tf`. That is the whole point of the arm: the
//   deserialization and the fan-out are paid **once**, by one process, whatever
//   N is.
//
// # How the bridge's cost stays inside the arm it serves
//
// A four-arm table in which one arm quietly runs an extra process for free is
// worse than the three-arm table it replaces. It does not: the bridge process
// emits the same stats block as every other process here, with `consumers 0`.
// `dds_report`'s aggregator sums `cpu_ns` and `pss_kib` over every process
// sharing an arm label and divides CPU by the **summed** consumer count, so a
// process that serves consumers without being one lands its whole cost in that
// arm, amortized over exactly the consumers it serves. No new column, no new
// protocol, and no way to leave it out by accident.
//
// `--mode tf_tree_bridge` is an explicit mode rather than `--consumers 0`
// because `--consumers 0` is refused below, and should stay refused: a query
// arm that measured nothing would otherwise report perfect latencies.
//
// # Where tf_tree is worse here, stated rather than footnoted (§9.3)
//
// The `tf_tree.processes` arm runs **N+1** processes to the tf2 arm's N, and
// that extra process is one an operator has to supervise, restart and watch.
// The `procs` column shows it. Its arena also costs a `memfd`, a rendezvous
// entry and a participant slot that no tf2 arm pays.
//
// # Measurement
//
// `measure.hpp`, which mirrors `crates/tf_tree_bench/src/mp.rs`: open-loop
// schedule (a stall must show up as latency, not as fewer samples), a `service`
// distribution for the engine's own cost and a `cycle` distribution for what the
// node experiences, whole-process CPU in nanoseconds, and PSS rather than summed
// RSS. That CPU reading is the one place `measure.hpp` stops mirroring `mp.rs`,
// and its header says why: the reading this file used to take covered the main
// thread alone, which is the one thread every arm here leaves asleep.

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
// **`#error`, not a runtime refusal**, for the reason `test_shared_arena.cpp`
// gives: `ros/build.sh` builds `libtf_tree_c` with `--features bridge,shm` and
// refuses to continue if `tft_tree_open` is missing, and the CMake package
// defines `TFT_HAVE_SHM` by probing that same archive. Arriving here means one
// of those two broke, and the symptom would otherwise be a `tf_tree.processes`
// arm that silently stopped being built while the other three kept reporting.
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
  /// How far behind "now" every query is aimed, in nanoseconds.
  ///
  /// 100 ms, and both arms use it. A query aimed at the present asks for a
  /// sample that has not arrived, which is an error path — and timing an error
  /// path is timing an error path, not an engine. `mp_compare.py` carries the
  /// same constant for the same reason.
  int64_t lag_ns = 100'000'000;
  /// `--mode tf_tree_bridge` only: keep serving this long after the measured
  /// window closes.
  ///
  /// The bridge and the consumers of its arm are launched together and both run
  /// `warmup + seconds`, but a consumer cannot start its warm-up until the
  /// rendezvous exists, so its window ends *later* than the bridge's by however
  /// long the bridge took to publish. A bridge that exited on its own schedule
  /// would leave the tail of every consumer's measured window reading an arena
  /// nobody is writing — which is fast, correct-looking and meaningless. The
  /// driver sets this; the linger is outside the bridge's own measured window,
  /// so it costs the arm's CPU column nothing.
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

/// Compile one plan per query pair, or explain which pair the arena refused.
///
/// **Frees what it compiled before it returns false**, so `out` is empty on
/// failure and neither caller has to remember. Both used to `return 1` on this
/// path with the plans compiled so far still in the vector — right beside the
/// attach path's `tft_tree_free(tree)`, which made the asymmetry read as an
/// oversight rather than as a decision about a process that is exiting anyway.
/// It is a benchmark and the leak was harmless; owning the plans in one place
/// is a line shorter than explaining that twice.
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
/// and emit the per-thread and per-process blocks.
///
/// Shared by `--mode tf_tree` and `--mode tf_tree_attach` so the two arms differ
/// in **where the arena came from** and in nothing else: the same schedule, the
/// same stamp policy, the same warm-up handling and the same instrument. §9.3's
/// "identical executor configuration" is code identity here, not a promise.
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

/// The bridge's own account of what it ingested. A run whose bridge dropped
/// everything would otherwise report beautiful latencies for an empty arena.
///
/// **That sentence is now true rather than aspirational.** These three numbers
/// were parsed by `dds_report` and written to `results.json` and gated
/// *nothing*: an arm whose bridge received zero transforms printed a clean row
/// and exit 0. `dds_report::check_structure` refuses it, so the counter the
/// comment above describes is the one that stops the run.
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
  if (!compile_plans(bridge.tree(), pairs, plans)) {return 1;}

  measure_tf_tree_consumers(plans, node, args);

  for (auto * p : plans) {tft_plan_free(p);}
  print_bridge_stats(bridge);
  return 0;
}

// ---------------------------------------------------------------------------
// tf_tree, across processes — `docs/decisions/0015` step 5
// ---------------------------------------------------------------------------

/// The rendezvous name both halves of the arm select by.
///
/// **`$TF_TREE_NAME`, not a flag**, and that is the whole reason the two halves
/// cannot drift apart. `tft_tree_open()` takes no name argument — the
/// environment selects the arena (`$TF_TREE_DOMAIN`, `$TF_TREE_NAME`,
/// `$TF_TREE_RUNTIME_DIR`), exactly as `tf_tree::open()` does. Giving the bridge
/// a `--arena-name` flag while the consumer read the environment would be two
/// sources of truth for one string, and the failure mode of their disagreeing is
/// a consumer that waits forever for a name nobody published.
std::string arena_name_from_env()
{
  const char * n = std::getenv("TF_TREE_NAME");
  return n == nullptr ? std::string() : std::string(n);
}

/// `tft_tree_open` until it succeeds or `timeout` passes; nullptr on timeout.
///
/// **The C ABI has no timeout parameter** — `Open::await_open` is Rust-only and
/// deliberately not exposed — so a bounded poll is the only shape available.
/// `ros/tf_tree_ros/test/test_shared_arena.cpp`'s `open_within` is the same
/// function for the same reason.
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

/// One bridge process: hosts §5.8 form 3 under a rendezvous name, serves the
/// arm's consumers, and runs no query threads of its own.
///
/// It reports `consumers 0` and the same `cpu_ns` / `pss_kib` block every other
/// process here reports, which is how its cost lands inside the arm rather than
/// beside it (see the file header).
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
  // Defaults everywhere else, for the reason `run_tf_tree` gives at length: a
  // bridge tuned for the benchmark would not be the bridge an operator deploys.
  o.arena_name = name;
  // A failure here is a `BridgeError` out of the constructor and there is no
  // fallback to a heap arena — `docs/decisions/0015`'s *Failure* section. It
  // propagates out of `main`'s catch as a non-zero exit, and `dds_bench.sh`
  // prints this process's stderr and stops the run, which is what must happen:
  // an arm whose bridge never started would otherwise be N consumers timing out
  // one after another.
  tf_tree_ros::BridgeHandle bridge(node.get(), o);

  std::thread spinner([&node]() {rclcpp::spin(node);});

  // The same warm-up and the same measured window as the consumers it serves,
  // so the CPU this reports is the CPU it spent while they were measuring.
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
/// process published, and runs `--consumers N` query threads on it.
///
/// **It hosts no bridge and no subscription to `/tf`.** That is the arm, and it
/// is also the one asymmetry against `tf2.processes` that is not an artifact of
/// the harness: a tf2 listener process deserializes every `/tf` message and
/// maintains its own cache, and this one does neither *because the architecture
/// under test does not require it to* — the bridge process in the same arm pays
/// that cost once, and its CPU and PSS are in the same row. `dds_report` prints
/// this in the table's own footer rather than leaving it here.
///
/// **It does construct and spin an rclcpp node**, and that is deliberate even
/// though nothing in this mode needs one. Two reasons, and the second is the
/// load-bearing one. First, the stamp: every other arm aims its queries with
/// `node->now()`, and a different clock would make this arm ask a different
/// question. Second, fairness of the PSS and CPU columns — a `tf2.processes`
/// consumer is an rclcpp node with a DDS participant, and a real deployment's
/// tf_tree consumer is one too, because a node that reads transforms exists to
/// do something else with them. Dropping the participant here would move
/// something like 10 MiB per process out of the arm and measure "no rclcpp"
/// rather than "no `/tf`", which is not the claim.
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
  // **The one process here that is not a consumer**, and the only place
  // `consumers 0` is legitimate. It is a mode rather than `--consumers 0`
  // precisely so the refusal below stays a refusal: a query arm that ran no
  // queries would report an empty histogram as a perfect one.
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
    // Stated in the output, because §9.3 requires the discarded warm-up window
    // to be reported rather than merely applied.
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
