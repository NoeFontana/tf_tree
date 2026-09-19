// `docs/PHASE4.md` §5.5 and `docs/decisions/0012` — the clock, from the ROS side
// of the seam. Two properties need a real `rclcpp::TimeSource` and `/clock`:
//
//   1. The authoritative path: rcl reports clock jumps (a looping bag, a sim
//      reset) to jump callbacks, so the bridge asks rather than infers
//      (`docs/decisions/0012`; inference survives only as L2 common-mode).
//   2. Which clock throttled diagnostics use: `node_->get_clock()` reads zero
//      until the first `/clock` and rewinds with a bag, silencing every
//      `RCLCPP_*_THROTTLE` in `bridge_handle.cpp` when they are needed.
//
// The engine's own rules are unit-tested in `crates/tf_tree_bridge` and
// `crates/tf_tree_c/tests/bridge.rs`.

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

/// Ten seconds of simulated time, and the five it rewinds to (50x §5.5's threshold).
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

/// Counts log records whose **format string** contains a needle at or above a
/// severity, forwarding every record to the prior handler. The format is the
/// literal in `bridge_handle.cpp`, so a match is that exact line. The handler is
/// process-global and not thread-safe: swap it from the test thread while no
/// bridge runs.
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

/// A node with `use_sim_time` true at construction via `parameter_overrides`;
/// enabling it later fires `RCL_ROS_TIME_ACTIVATED` at an existing bridge.
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

/// A `/clock` that goes backwards stops the bridge with no publisher stamp
/// consulted (§5.5's case, reported by the time source). `/tf` is silent, so the
/// halt can only come from rcl's jump callback, latched and drained on the
/// ingest thread from `run`'s loop (not only `ingest`).
///
/// Mutant (applied, observed to fail): delete the `drain_time_jump()` at the
/// bottom of `BridgeHandle::run`'s loop; `clock_resets` stays 0.
/// Mutant (stated): drop the handler returned by `register_jump_callback`
/// instead of storing it (rclcpp holds a `weak_ptr`).
/// Mutant (stated, not applied): call `tft_bridge_note_time_jump` from the
/// post-callback; release returns `TFT_ERR_WRONG_THREAD`, debug aborts.
TEST(ClockTest, a_backward_clock_jump_reported_by_the_time_source_stops_the_bridge)
{
  const std::string topic = "/tf_clock_jump";
  auto node = sim_time_node("clock_jump_bridge");

  auto clock_node = std::make_shared<rclcpp::Node>("clock_jump_source");
  auto clock_pub =
    clock_node->create_publisher<rosgraph_msgs::msg::Clock>("/clock", rclcpp::ClockQoS());

  // The edgeless sentence: a jump reported by the time source has no sample, so
  // `parent`/`child` are empty. Counting the format string tells the two call
  // sites apart.
  start_counting("ingest bridge HALTED: ", RCUTILS_LOG_SEVERITY_FATAL);

  uint64_t resets = 0;
  uint64_t refused = 0;
  {
    tf_tree_ros::BridgeHandle bridge(node.get(), options_on(topic));

    // Simulated time runs forward first so the rewind is a rewind; republished
    // because `/clock` is `KeepLast(1)` volatile and discovery is not instant.
    ASSERT_TRUE(
      wait_for(
        [&] {
          clock_pub->publish(clock_at(kSimStart));
          return node->get_clock()->now().nanoseconds() >= kSimStart;
        },
        20s)) << "the node's ROS clock never followed /clock; use_sim_time did not take effect, "
                 "so there was no clock to rewind";

    // Starting simulated time is not a jump: the first `/clock` moves time from 0
    // to the epoch, and rcl calls jump callbacks for every `/clock` message, so
    // a finite `min_forward` would stop the bridge at startup. The sleep lets the
    // 50 ms drain run.
    //
    // Mutant: `threshold.min_forward.nanoseconds = 1` in `register_jump_callback`;
    // `clock_resets` is 1 (the other expectations still pass).
    std::this_thread::sleep_for(500ms);
    ASSERT_EQ(bridge.stats().clock_resets, 0u)
      << "simulated time merely starting was reported as a clock jump; min_forward must stay "
         "disabled (docs/decisions/0012)";

    // The loop point: five seconds backwards.
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

    // The stop is a stop: a later transform is refused, proving the reported jump
    // reaches the same latch as the per-sample path.
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

/// A throttled diagnostic still prints when sim time has not started. With no
/// `/clock` publisher `node_->get_clock()->now()` is 0, so `now >= last_logged +
/// period` is false forever and every throttled line was silent for the whole
/// sim boot. The steady clock has no such state. A frame name `"/"` (an
/// unsubstituted launch variable) reaches the `BAD_NAME` line cheaply.
///
/// Mutant (applied, observed to fail in 27 ms): restore `*node_->get_clock()` as
/// the `BAD_NAME` throttle clock in `BridgeHandle::report`; `dropped_bad_name`
/// still climbs but the warning count is 0.
TEST(ClockTest, a_throttled_diagnostic_is_not_silenced_by_sim_time_that_never_started)
{
  const std::string topic = "/tf_clock_throttle";
  // Sim time on and nothing publishes `/clock`: the first seconds of every sim launch.
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
