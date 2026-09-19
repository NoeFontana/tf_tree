// `docs/PHASE4.md` §5: the ingest path end to end over a real DDS. The engine
// rules are in `crates/tf_tree_bridge` and `crates/tf_tree_c/tests/bridge.rs`.

#include <atomic>
#include <chrono>
#include <memory>
#include <string>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include <rclcpp/rclcpp.hpp>
#include <rcutils/logging.h>
#include <tf2_msgs/msg/tf_message.hpp>

#include "tf_tree_ros/bridge_handle.hpp"

namespace
{

/// One dynamic and one static edge, as in `crates/tf_tree_c/tests/bridge.rs`.
constexpr const char * kTopology = R"(
[[edge]]
parent = "odom"
child = "base_link"
kind = "dynamic"
capacity = 256

[[edge]]
parent = "base_link"
child = "lidar"
kind = "static"
pose = [0.9659258262890683, 0.0, 0.0, 0.25881904510252074, 0.35, -0.02, 0.61]
)";

/// A 30 degree yaw with a unique translation, so identity or the static pose fails.
constexpr double kQw = 0.9659258262890683;
constexpr double kQz = 0.25881904510252074;
constexpr double kTx = 1.5;
constexpr double kTy = -2.25;
constexpr double kTz = 0.75;

constexpr int64_t kStamp = 1'000'000'000LL;

geometry_msgs::msg::TransformStamped make_transform(
  const std::string & parent, const std::string & child, int64_t stamp_ns)
{
  geometry_msgs::msg::TransformStamped t;
  t.header.frame_id = parent;
  t.child_frame_id = child;
  t.header.stamp.sec = static_cast<int32_t>(stamp_ns / 1000000000LL);
  t.header.stamp.nanosec = static_cast<uint32_t>(stamp_ns % 1000000000LL);
  t.transform.rotation.w = kQw;
  t.transform.rotation.x = 0.0;
  t.transform.rotation.y = 0.0;
  t.transform.rotation.z = kQz;
  t.transform.translation.x = kTx;
  t.transform.translation.y = kTy;
  t.transform.translation.z = kTz;
  return t;
}

/// Publish `msg` until `want` transforms are offered or the deadline passes.
bool pump_until(
  const rclcpp::Publisher<tf2_msgs::msg::TFMessage>::SharedPtr & pub,
  const tf2_msgs::msg::TFMessage & msg, const tf_tree_ros::BridgeHandle & bridge, uint64_t want,
  std::chrono::seconds timeout)
{
  const auto deadline = std::chrono::steady_clock::now() + timeout;
  while (std::chrono::steady_clock::now() < deadline) {
    pub->publish(msg);
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
    if (bridge.stats().transforms >= want) {
      return true;
    }
  }
  return false;
}

class IngestTest : public ::testing::Test
{
protected:
  /// A topic per test, never the real `/tf`, so other ROS processes cannot move the counters.
  void SetUp() override
  {
    topic_ = std::string("/tf_ingest_") + ::testing::UnitTest::GetInstance()->
      current_test_info()->name();
    node_ = std::make_shared<rclcpp::Node>("tf_tree_ingest_test");
    publisher_node_ = std::make_shared<rclcpp::Node>("tf_broadcaster_under_test");
    pub_ = publisher_node_->create_publisher<tf2_msgs::msg::TFMessage>(
      topic_, rclcpp::QoS(rclcpp::KeepLast(100)).reliable());
  }

  tf_tree_ros::BridgeOptions options() const
  {
    tf_tree_ros::BridgeOptions o;
    o.topology_toml = kTopology;
    o.tf_topic = topic_;
    o.tf_static_topic = topic_ + "_static";
    return o;
  }

  std::string topic_;
  rclcpp::Node::SharedPtr node_;
  rclcpp::Node::SharedPtr publisher_node_;
  rclcpp::Publisher<tf2_msgs::msg::TFMessage>::SharedPtr pub_;
};

/// A `/tf` transform from another participant reads back through `tft_plan_at`
/// with the pose sent (QoS and the unpack into `tft_bridge_sample`).
TEST_F(IngestTest, an_applied_transform_reads_back_with_the_pose_that_was_sent)
{
  tf_tree_ros::BridgeHandle bridge(node_.get(), options());

  tf2_msgs::msg::TFMessage msg;
  msg.transforms.push_back(make_transform("odom", "base_link", kStamp));
  ASSERT_TRUE(pump_until(pub_, msg, bridge, 1, std::chrono::seconds(20)));

  const auto stats = bridge.stats();
  EXPECT_GE(stats.applied, 1u);
  EXPECT_EQ(stats.dropped_undeclared, 0u);
  EXPECT_EQ(stats.dropped_bad_pose, 0u);

  tft_plan * plan = nullptr;
  ASSERT_EQ(tft_plan_create(bridge.tree(), "odom", "base_link", &plan), TFT_OK);

  double pose[7] = {0};
  ASSERT_EQ(tft_plan_at(plan, kStamp, TFT_LAYOUT_QVEC7_WXYZ, pose), TFT_OK);
  tft_plan_free(plan);

  EXPECT_NEAR(pose[0], kQw, 1e-12);
  EXPECT_NEAR(pose[1], 0.0, 1e-12);
  EXPECT_NEAR(pose[2], 0.0, 1e-12);
  EXPECT_NEAR(pose[3], kQz, 1e-12);
  EXPECT_NEAR(pose[4], kTx, 1e-12);
  EXPECT_NEAR(pose[5], kTy, 1e-12);
  EXPECT_NEAR(pose[6], kTz, 1e-12);
}

/// An undeclared edge is dropped and counted (§5.8's amendment) while declared
/// ones keep being ingested.
TEST_F(IngestTest, an_undeclared_edge_is_dropped_without_stopping_the_declared_one)
{
  tf_tree_ros::BridgeHandle bridge(node_.get(), options());

  tf2_msgs::msg::TFMessage msg;
  msg.transforms.push_back(make_transform("map", "odom", kStamp));
  msg.transforms.push_back(make_transform("odom", "base_link", kStamp));
  ASSERT_TRUE(pump_until(pub_, msg, bridge, 2, std::chrono::seconds(20)));

  const auto stats = bridge.stats();
  EXPECT_GE(stats.dropped_undeclared, 1u);
  EXPECT_GE(stats.applied, 1u);
  // §5.9's ledger balances on a healthy bridge and on this one.
  EXPECT_EQ(
    stats.applied + stats.rejected_by_arena + stats.static_verified + stats.dropped_authority +
    stats.dropped_non_monotonic + stats.dropped_bad_name + stats.dropped_kind_change +
    stats.dropped_undeclared + stats.dropped_bad_pose + stats.refused_after_halt,
    stats.transforms);
}

/// A config the engine will not build fails in the constructor (§5.5).
TEST_F(IngestTest, a_config_that_does_not_parse_throws_from_the_constructor)
{
  tf_tree_ros::BridgeOptions o = options();
  o.topology_toml = "[[edge]]\nparent = \"a\"\n";  // no child, no kind
  EXPECT_THROW(tf_tree_ros::BridgeHandle(node_.get(), o), tf_tree_ros::BridgeError);
}

/// Form 3 refuses an empty topology, as forms 1 and 2 do.
TEST_F(IngestTest, form_3_refuses_a_topology_that_declares_no_edges)
{
  tf_tree_ros::BridgeOptions o = options();
  o.topology_toml = "";
  EXPECT_THROW(tf_tree_ros::BridgeHandle(node_.get(), o), tf_tree_ros::BridgeError);
}

/// §5.6's remap table crosses the C boundary complete, and a prefixed arena is
/// the one a consumer looks up. `tft_bridge_get_remap` pointers are overwritten
/// by the next call, so each row must be copied.
TEST_F(IngestTest, a_tf_prefix_is_reported_as_a_remap_table_and_renames_the_arena)
{
  tf_tree_ros::BridgeOptions o = options();
  o.tf_prefix = "robot1";
  tf_tree_ros::BridgeHandle bridge(node_.get(), o);

  // Every declared frame, rewritten, complete before the first message.
  const std::vector<std::pair<std::string, std::string>> expected{
    {"odom", "robot1/odom"},
    {"base_link", "robot1/base_link"},
    {"lidar", "robot1/lidar"},
  };
  // Exact and in file order, so a loop that stored pointers fails.
  EXPECT_EQ(bridge.remap(), expected);

  // The wire is normalized too, so an unprefixed publisher lands on the prefixed edge.
  tf2_msgs::msg::TFMessage msg;
  msg.transforms.push_back(make_transform("odom", "base_link", kStamp));
  ASSERT_TRUE(pump_until(pub_, msg, bridge, 1, std::chrono::seconds(20)));
  EXPECT_GE(bridge.stats().applied, 1u);
  EXPECT_EQ(bridge.stats().dropped_undeclared, 0u);

  tft_plan * plan = nullptr;
  ASSERT_EQ(
    tft_plan_create(bridge.tree(), "robot1/odom", "robot1/base_link", &plan), TFT_OK);
  tft_plan_free(plan);
}

namespace
{
/// Counts `FATAL` lines, forwarding every record to the prior handler. The
/// handler is process-global: swap it while no bridge runs.
std::atomic<int> g_fatal_lines{0};
rcutils_logging_output_handler_t g_previous_handler = nullptr;

void counting_handler(
  const rcutils_log_location_t * location, int severity, const char * name,
  rcutils_time_point_value_t timestamp, const char * format, va_list * args)
{
  if (severity >= RCUTILS_LOG_SEVERITY_FATAL) {
    g_fatal_lines.fetch_add(1);
  }
  // Forwarded exactly once: `args` is a `va_list` and consuming it twice is UB.
  if (g_previous_handler != nullptr) {
    g_previous_handler(location, severity, name, timestamp, format, args);
  }
}
}  // namespace

/// Two dynamic edges with one publisher each, the shape of a `/clock` reset;
/// local because another test expects `map -> odom` undeclared.
constexpr const char * kTwoOwnerTopology = R"(
[[edge]]
parent = "odom"
child = "base_link"
kind = "dynamic"
capacity = 256

[[edge]]
parent = "map"
child = "odom"
kind = "dynamic"
capacity = 256
)";

/// One node publishing one edge, so two are two *publishers* for §5.3 and §5.5.
class OneEdgeBroadcaster
{
public:
  OneEdgeBroadcaster(
    const std::string & name, const std::string & topic, std::string parent, std::string child)
  : node_(std::make_shared<rclcpp::Node>(name)),
    pub_(node_->create_publisher<tf2_msgs::msg::TFMessage>(
        topic, rclcpp::QoS(rclcpp::KeepLast(100)).reliable())),
    parent_(std::move(parent)), child_(std::move(child))
  {
  }

  void publish_at(int64_t stamp_ns)
  {
    tf2_msgs::msg::TFMessage msg;
    msg.transforms.push_back(make_transform(parent_, child_, stamp_ns));
    pub_->publish(msg);
  }

private:
  rclcpp::Node::SharedPtr node_;
  rclcpp::Publisher<tf2_msgs::msg::TFMessage>::SharedPtr pub_;
  std::string parent_;
  std::string child_;
};

/// A halted bridge says so once, not once per transform (§5.4 "rate-limited"),
/// and it takes two publishers stepping together to halt it
/// (`docs/decisions/0012`). Two nodes, two edges, one shared -5 s step.
TEST_F(IngestTest, a_clock_reset_is_announced_once_and_not_once_per_refused_transform)
{
  g_fatal_lines.store(0);
  g_previous_handler = rcutils_logging_get_output_handler();
  rcutils_logging_set_output_handler(counting_handler);

  uint64_t refused = 0;
  uint64_t applied = 0;
  uint64_t resets_while_healthy = 0;
  {
    tf_tree_ros::BridgeOptions o = options();
    o.topology_toml = kTwoOwnerTopology;
    tf_tree_ros::BridgeHandle bridge(node_.get(), o);

    OneEdgeBroadcaster wheels("clock_reset_wheel_driver", topic_, "odom", "base_link");
    OneEdgeBroadcaster localizer("clock_reset_localizer", topic_, "map", "odom");

    // Stamps use the bridge's steady clock so each offset stays flat.
    const auto period = std::chrono::milliseconds(20);
    const auto t0 = std::chrono::steady_clock::now();
    int64_t rewind = 0;
    const auto stamp_now = [&t0, &rewind] {
        const int64_t elapsed =
          std::chrono::duration_cast<std::chrono::nanoseconds>(
          std::chrono::steady_clock::now() - t0).count();
        return 10 * kStamp + elapsed + rewind;
      };

    // Both publishers discovered and applying, with offsets established.
    const auto warm = std::chrono::steady_clock::now() + std::chrono::seconds(20);
    while (std::chrono::steady_clock::now() < warm) {
      const int64_t stamp = stamp_now();
      wheels.publish_at(stamp);
      localizer.publish_at(stamp);
      std::this_thread::sleep_for(period);
      applied = bridge.stats().applied;
      if (applied >= 20) {
        break;
      }
    }
    resets_while_healthy = bridge.stats().clock_resets;

    // The reset: one shared step, far past §5.5's 100 ms threshold.
    rewind = -5 * kStamp;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(20);
    while (std::chrono::steady_clock::now() < deadline) {
      const int64_t stamp = stamp_now();
      wheels.publish_at(stamp);
      localizer.publish_at(stamp);
      std::this_thread::sleep_for(period);
      refused = bridge.stats().refused_after_halt;
      if (refused >= 5) {
        break;
      }
    }
  }
  const int fatal = g_fatal_lines.load();
  rcutils_logging_set_output_handler(g_previous_handler);

  ASSERT_GE(applied, 20u)
    << "the two broadcasters never got going, so there was no steady state to step away from";
  // Asserted first: a healthy-phase halt would satisfy the rest wrongly.
  ASSERT_EQ(resets_while_healthy, 0u)
    << "the bridge decided the clock had moved while both publishers were healthy and their "
       "stamps were tracking wall time";
  ASSERT_GE(refused, 5u)
    << "two publishers stepped by the same -5 s and the bridge did not stop. Either the receipt "
       "clock is not reaching the sample, or §5.3's attribution did not resolve the two GIDs to "
       "two distinct nodes — in which case they are one publisher to the detector and one witness "
       "is never enough";
  EXPECT_EQ(fatal, 1)
    << "the halt was logged " << fatal << " times against " << refused
    << " refused transforms; §5.4 requires the diagnostic be rate-limited";
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
