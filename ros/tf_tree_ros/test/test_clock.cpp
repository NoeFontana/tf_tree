// `docs/PHASE4.md` §5.5 and `docs/decisions/0012` — the clock, from the ROS
// side of the seam.
//
// Two properties live here and nowhere else, because both of them need a real
// `rclcpp::TimeSource` and a real `/clock`:
//
//   1. **The authoritative path.** ROS 2 *publishes* clock jumps. A bag that
//      loops and a simulator that resets both call `rcl_set_ros_time_override`
//      with a time behind the one before it, and rcl reports that to every
//      registered jump callback. Inferring the same fact from the stamps of the
//      publishers under suspicion is what three successive versions of this rule
//      did, and all three were wrong; the engine keeps that inference as a
//      fallback, and this file is about not needing it.
//
//   2. **Which clock the diagnostics are rate-limited on.** Every
//      `RCLCPP_*_THROTTLE` in `bridge_handle.cpp` compares `clock.now()` against
//      a remembered timestamp. Given `node_->get_clock()` under `use_sim_time`
//      that clock reads **zero** until the first `/clock` message and *rewinds*
//      when a bag loops — so the throttle is silent over exactly the boot window
//      a misconfigured bridge is diagnosed in, and silent again for the duration
//      of a rewind about which it is the only thing that would speak. Nothing in
//      the Rust half can see this: it is a property of a C++ macro and a ROS
//      parameter.
//
// The engine's own rules — when a jump halts, what evidence promotes a step,
// how a per-edge guard drops — are unit-tested in `crates/tf_tree_bridge` and
// `crates/tf_tree_c/tests/bridge.rs` and are not retested here.

#include <atomic>
#include <chrono>
#include <cstring>
#include <memory>
#include <string>
#include <thread>
#include <vector>

#include <gtest/gtest.h>

#include <rclcpp/rclcpp.hpp>
#include <rcutils/logging.h>
#include <rosgraph_msgs/msg/clock.hpp>
#include <tf2_msgs/msg/tf_message.hpp>

#include "tf_tree_ros/bridge_handle.hpp"

namespace
{

using namespace std::chrono_literals;

constexpr const char * kTopology = R"(
[[edge]]
parent = "odom"
child = "base_link"
kind = "dynamic"
capacity = 256
)";

/// Ten seconds of simulated time, and the five it rewinds to. Fifty times
/// §5.5's 100 ms threshold, so nothing here is decided by a boundary.
constexpr int64_t kSimStart = 10'000'000'000LL;
constexpr int64_t kSimRewound = 5'000'000'000LL;

rosgraph_msgs::msg::Clock clock_at(int64_t ns)
{
  rosgraph_msgs::msg::Clock m;
  m.clock.sec = static_cast<int32_t>(ns / 1000000000LL);
  m.clock.nanosec = static_cast<uint32_t>(ns % 1000000000LL);
  return m;
}

geometry_msgs::msg::TransformStamped transform_at(
  const std::string & parent, const std::string & child, int64_t stamp_ns)
{
  geometry_msgs::msg::TransformStamped t;
  t.header.frame_id = parent;
  t.child_frame_id = child;
  t.header.stamp.sec = static_cast<int32_t>(stamp_ns / 1000000000LL);
  t.header.stamp.nanosec = static_cast<uint32_t>(stamp_ns % 1000000000LL);
  t.transform.rotation.w = 1.0;
  return t;
}

template<typename F>
bool wait_for(F predicate, std::chrono::milliseconds timeout)
{
  const auto deadline = std::chrono::steady_clock::now() + timeout;
  while (std::chrono::steady_clock::now() < deadline) {
    if (predicate()) {
      return true;
    }
    std::this_thread::sleep_for(20ms);
  }
  return predicate();
}

/// Counts log records whose **format string** contains a needle, at or above a
/// severity, and forwards every record to the handler that was installed
/// before.
///
/// Matching on the format rather than on the rendered text is what makes this
/// specific: the format is the literal in `bridge_handle.cpp`, so a match is
/// that exact line and not a coincidence in a frame name. Matching on the
/// logger name would not distinguish the seven throttled lines from each other.
///
/// `rcutils_logging_set_output_handler` is process-global and documented not
/// thread-safe, so it is installed and removed from the test thread while no
/// bridge is running; only the counter is written from the ingest thread.
std::atomic<int> g_matches{0};
std::atomic<int> g_min_severity{RCUTILS_LOG_SEVERITY_WARN};
const char * g_needle = "";
rcutils_logging_output_handler_t g_previous_handler = nullptr;

void counting_handler(
  const rcutils_log_location_t * location, int severity, const char * name,
  rcutils_time_point_value_t timestamp, const char * format, va_list * args)
{
  if (severity >= g_min_severity.load() && format != nullptr &&
    std::strstr(format, g_needle) != nullptr)
  {
    g_matches.fetch_add(1);
  }
  // Forwarded exactly once: `args` is a `va_list` and consuming it twice is UB.
  if (g_previous_handler != nullptr) {
    g_previous_handler(location, severity, name, timestamp, format, args);
  }
}

void start_counting(const char * needle, int min_severity)
{
  g_matches.store(0);
  g_needle = needle;
  g_min_severity.store(min_severity);
  g_previous_handler = rcutils_logging_get_output_handler();
  rcutils_logging_set_output_handler(counting_handler);
}

int stop_counting()
{
  rcutils_logging_set_output_handler(g_previous_handler);
  return g_matches.load();
}

/// A node with `use_sim_time` already true at construction.
///
/// Set through `parameter_overrides` rather than with `set_parameter` after the
/// fact, and that ordering is load-bearing: rclcpp's `TimeSource` calls
/// `rcl_enable_ros_time_override` while the node is being built, which is itself
/// a `RCL_ROS_TIME_ACTIVATED` jump. Turning sim time on *later* would fire that
/// at a `BridgeHandle` that already exists, halting it for a reason this test is
/// not about — correctly, but not usefully.
rclcpp::Node::SharedPtr sim_time_node(const std::string & name)
{
  rclcpp::NodeOptions o;
  o.parameter_overrides({rclcpp::Parameter("use_sim_time", true)});
  return std::make_shared<rclcpp::Node>(name, o);
}

tf_tree_ros::BridgeOptions options_on(const std::string & topic)
{
  tf_tree_ros::BridgeOptions o;
  o.topology_toml = kTopology;
  o.tf_topic = topic;
  o.tf_static_topic = topic + "_static";
  return o;
}

/// **A `/clock` that goes backwards stops the bridge, with no publisher stamp
/// consulted at all.**
///
/// This is §5.5's case — a bag loop, a sim reset — reported by the time source
/// instead of guessed at. Nothing publishes `/tf` before the rewind, so there
/// are no stamps to infer from and no quorum that could be reached: the halt
/// here can only have come from `rcl`'s jump callback. That is the whole point
/// of the tier, and it is why the assertion is made with the `/tf` topic silent.
///
/// It also pins the two things about the hand-off that cannot be seen from
/// Rust. The callback fires on the `TimeSource`'s own `/clock` thread — with
/// `NodeOptions::use_clock_thread` defaulting to true it is never the bridge
/// thread — and `tft_bridge_*` is affinity-checked, so the jump has to be
/// latched and drained on the ingest thread. And it must be drained from
/// `run`'s loop rather than only from `ingest`, because at this moment `/tf`
/// carries nothing: a drain that waited for the next transform would wait
/// forever on a bag between takes.
///
/// **Mutant — APPLIED, in `docker/tf2`, and observed:** in `BridgeHandle::run`,
/// delete the `drain_time_jump()` call at the bottom of the loop, leaving only
/// the one at the top of `ingest`. Nothing publishes `/tf` here, so nothing ever
/// drains, `clock_resets` stays 0 and the first wait times out. `just ros-test`
/// reported `[  FAILED  ]
/// ClockTest.a_backward_clock_jump_reported_by_the_time_source_stops_the_bridge
/// (20554 ms)`, `83% tests passed, 1 tests failed out of 6`. That is the whole
/// point of this suite: a bridge whose `/tf` has gone silent is exactly when the
/// time source's own report is the only signal there is, and draining only from
/// `ingest` means it is never applied. Source restored byte-identical afterwards.
/// **Mutant:** in `register_jump_callback`, drop the
/// returned handler on the floor instead of storing it in `jump_handler_`.
/// rclcpp holds only a `weak_ptr`, so the callback is unregistered immediately
/// and silently — the same failure, with nothing anywhere reporting it.
/// (Stated, not applied.)
/// **Mutant:** call `tft_bridge_note_time_jump` directly from the post-callback.
/// Under `ros/build.sh`'s release build of `tf_tree_c` that returns
/// `TFT_ERR_WRONG_THREAD`, the jump is dropped, and this test times out; a debug
/// build of the same code aborts the process instead. (Stated, and deliberately
/// left that way: the debug half of that sentence takes the whole test process
/// down rather than failing one case, so applying it would prove the affinity
/// rule by destroying the run that was meant to observe it.)
TEST(ClockTest, a_backward_clock_jump_reported_by_the_time_source_stops_the_bridge)
{
  const std::string topic = "/tf_clock_jump";
  auto node = sim_time_node("clock_jump_bridge");

  auto clock_node = std::make_shared<rclcpp::Node>("clock_jump_source");
  auto clock_pub =
    clock_node->create_publisher<rosgraph_msgs::msg::Clock>("/clock", rclcpp::ClockQoS());

  // **The needle is the EDGELESS form of the sentence**, `"ingest bridge
  // HALTED: "` and not `"ingest bridge HALTED on "`. A jump reported by the time
  // source is a decision taken with no sample in hand, so the ABI leaves
  // `parent` and `child` at its documented "does not apply to this outcome"
  // empty string — and the edge-shaped sentence renders that as
  // `HALTED on  -> : …`, two empty conversions in the middle of the one line an
  // operator gets. Counting the format string rather than the rendered text is
  // what lets this distinguish the two call sites at all.
  start_counting("ingest bridge HALTED: ", RCUTILS_LOG_SEVERITY_FATAL);

  uint64_t resets = 0;
  uint64_t refused = 0;
  {
    tf_tree_ros::BridgeHandle bridge(node.get(), options_on(topic));

    // Simulated time runs forward first, so the rewind below is a rewind rather
    // than the clock starting. Republished because `/clock` is `KeepLast(1)`
    // volatile and DDS discovery is not instant.
    ASSERT_TRUE(
      wait_for(
        [&] {
          clock_pub->publish(clock_at(kSimStart));
          return node->get_clock()->now().nanoseconds() >= kSimStart;
        },
        20s)) << "the node's ROS clock never followed /clock; use_sim_time did not take effect, "
                 "so there was no clock to rewind";

    // **Starting simulated time is not a jump**, and this is the assertion that
    // says so. The first `/clock` message moves ROS time from 0 to the
    // simulation's epoch — here ten seconds, on a real robot's bag the better
    // part of a decade — and rcl reports that to any callback registered with a
    // finite `min_forward`, because it calls them from `set_ros_time_override`
    // for *every* `/clock` message rather than only for discontinuities. A
    // bridge that treated it as authoritative would stop before ingesting its
    // first transform, on every simulated deployment.
    //
    // The sleep is not a settling hack for the assertion below it: the drain
    // runs once per 50 ms poll of `run`'s loop, so without it this reads a
    // counter that has not had the chance to move and would pass either way.
    //
    // **Mutant:** set `threshold.min_forward.nanoseconds` to 1 in
    // `register_jump_callback`. The 0 -> 10 s start is reported as a forward
    // jump, the bridge halts here, and `clock_resets` is 1 — note that every
    // other expectation in this test would still pass, which is precisely why
    // the check has to be here and not inferred from the ones below.
    std::this_thread::sleep_for(500ms);
    ASSERT_EQ(bridge.stats().clock_resets, 0u)
      << "simulated time merely starting was reported as a clock jump; min_forward must stay "
         "disabled (docs/decisions/0012)";

    // The loop point. Five seconds backwards, fifty times §5.5's threshold.
    ASSERT_TRUE(
      wait_for(
        [&] {
          clock_pub->publish(clock_at(kSimRewound));
          resets = bridge.stats().clock_resets;
          return resets >= 1;
        },
        20s)) << "/clock went backwards by five seconds and the bridge did not notice. Either no "
                 "jump callback was registered, or the jump was never drained onto the ingest "
                 "thread. clock_resets=" << bridge.stats().clock_resets;

    // And the stop is a *stop*: a transform offered afterwards is refused,
    // whatever its stamp. This is the half that proves the authoritative path
    // reaches the same latch the per-sample path does, rather than merely
    // moving a counter.
    auto tf_node = std::make_shared<rclcpp::Node>("clock_jump_broadcaster");
    auto tf_pub = tf_node->create_publisher<tf2_msgs::msg::TFMessage>(
      topic, rclcpp::QoS(rclcpp::KeepLast(100)).reliable());
    tf2_msgs::msg::TFMessage msg;
    msg.transforms.push_back(transform_at("odom", "base_link", kSimStart));

    EXPECT_TRUE(
      wait_for(
        [&] {
          tf_pub->publish(msg);
          refused = bridge.stats().refused_after_halt;
          return refused >= 1;
        },
        20s)) << "the bridge counted a clock reset but kept accepting transforms: the halt from "
                 "tft_bridge_note_time_jump is not arming the latch tft_bridge_offer checks";
  }
  const int halts = stop_counting();

  EXPECT_GE(resets, 1u);
  EXPECT_EQ(halts, 1)
    << "the stop was announced " << halts << " times; §5.4 requires it be loud and rate-limited, "
    << "and `out.first_time` is 1 exactly once";
}

/// **A throttled diagnostic still prints when simulated time has not started.**
///
/// `RCLCPP_WARN_THROTTLE` expands to `now >= last_logged + period` over a
/// `last_logged` that starts at zero. Under `use_sim_time` with no `/clock`
/// publisher, `node_->get_clock()->now()` is **0**, the comparison is
/// `0 >= 0 + 5000000000`, and it is false forever — so every throttled line in
/// `report()` was suppressed for the entire boot of every simulated deployment.
/// Not rate-limited: *silent*. The steady clock has no such state: it is
/// monotonic since boot and independent of every ROS parameter.
///
/// A frame name that does not normalize is the cheapest way to reach one of
/// those lines. `"/"` is a bare leading slash with nothing after it — §5.6's
/// rule strips one slash and refuses what is left — and it is not hypothetical:
/// it is what a launch file with an unsubstituted variable publishes.
///
/// **Mutant — APPLIED, in `docker/tf2`, and observed:** put
/// `*node_->get_clock()` back as the clock of the `BAD_NAME` throttle in
/// `BridgeHandle::report`. `dropped_bad_name` still climbs, so the counter half
/// of this test passes and the ledger still balances — and the warning count
/// falls to zero, which is the whole finding: the fault was counted and never
/// spoken about. `just ros-test` reported `[  FAILED  ]
/// ClockTest.a_throttled_diagnostic_is_not_silenced_by_sim_time_that_never_started
/// (27 ms)`, `83% tests passed, 1 tests failed out of 6`.
///
/// Note the 27 ms against the other two mutants' 20 s: this one fails
/// *immediately*, on the first message, because under `use_sim_time` with no
/// `/clock` publisher `now()` is 0 and rcutils' `now >= last_logged + duration`
/// is false forever. Nothing is slow about it — the line simply never exists.
/// Source restored byte-identical afterwards.
TEST(ClockTest, a_throttled_diagnostic_is_not_silenced_by_sim_time_that_never_started)
{
  const std::string topic = "/tf_clock_throttle";
  // Sim time on, and **nothing publishes `/clock`** — which is not a contrived
  // state but the first seconds of every simulated launch, and the seconds in
  // which a misconfigured bridge is diagnosed.
  auto node = sim_time_node("clock_throttle_bridge");
  ASSERT_EQ(node->get_clock()->now().nanoseconds(), 0)
    << "something else in this process is publishing /clock, so this test cannot mean what it says";

  auto pub_node = std::make_shared<rclcpp::Node>("clock_throttle_broadcaster");
  auto pub = pub_node->create_publisher<tf2_msgs::msg::TFMessage>(
    topic, rclcpp::QoS(rclcpp::KeepLast(100)).reliable());

  start_counting("does not normalize", RCUTILS_LOG_SEVERITY_WARN);

  uint64_t bad = 0;
  {
    tf_tree_ros::BridgeHandle bridge(node.get(), options_on(topic));

    tf2_msgs::msg::TFMessage msg;
    msg.transforms.push_back(transform_at("odom", "/", 1'000'000'000LL));
    ASSERT_TRUE(
      wait_for(
        [&] {
          pub->publish(msg);
          bad = bridge.stats().dropped_bad_name;
          return bad >= 1;
        },
        20s)) << "the unnormalizable name never reached the bridge, so nothing was throttled";
  }
  const int warnings = stop_counting();

  EXPECT_GE(bad, 1u);
  EXPECT_GE(warnings, 1)
    << "the bridge dropped " << bad
    << " transform(s) for an unnormalizable frame name and said nothing: the throttle is being "
       "rate-limited on a clock that reads zero";
}

}  // namespace

int main(int argc, char ** argv)
{
  ::testing::InitGoogleTest(&argc, argv);
  rclcpp::init(argc, argv);
  const int rc = RUN_ALL_TESTS();
  rclcpp::shutdown();
  return rc;
}
