// `docs/PHASE4.md` §5 — the ingest path end to end, over a real DDS.
//
// Everything §5 *judges* is already tested in `crates/tf_tree_bridge` and
// `crates/tf_tree_c/tests/bridge.rs`, on every `just test`. What is untestable
// without a middleware, and therefore what is here, is the half this package
// adds: that a `tf2_msgs/TFMessage` published by another participant arrives,
// unpacks into the POD sample in the right order, and lands in the arena where
// a reader can find it.

#include <chrono>
#include <memory>
#include <string>

#include <gtest/gtest.h>

#include <rclcpp/rclcpp.hpp>
#include <tf2_msgs/msg/tf_message.hpp>

#include "tf_tree_ros/bridge_handle.hpp"

namespace
{

/// One dynamic edge and one static one — the same shape
/// `crates/tf_tree_c/tests/bridge.rs` uses, so a failure here is about the ROS
/// half rather than about the config parser.
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

/// A 30 degree yaw with a translation nothing else in the fixture shares, so a
/// read-back that returns identity — or the static edge's pose — fails rather
/// than coincidentally passing.
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

/// Publish `msg` until the bridge reports at least `want` transforms offered,
/// or the deadline passes.
///
/// Republishing rather than publishing once and sleeping is deliberate: DDS
/// discovery between two participants in one process is not instant, and a
/// single publish into an undiscovered subscription is simply lost. The test
/// would then be a discovery-latency measurement wearing an ingest test's name.
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
  void SetUp() override
  {
    node_ = std::make_shared<rclcpp::Node>("tf_tree_ingest_test");
    publisher_node_ = std::make_shared<rclcpp::Node>("tf_broadcaster_under_test");
    pub_ = publisher_node_->create_publisher<tf2_msgs::msg::TFMessage>(
      "/tf", rclcpp::QoS(rclcpp::KeepLast(100)).reliable());
  }

  tf_tree_ros::BridgeOptions options() const
  {
    tf_tree_ros::BridgeOptions o;
    o.topology_toml = kTopology;
    return o;
  }

  rclcpp::Node::SharedPtr node_;
  rclcpp::Node::SharedPtr publisher_node_;
  rclcpp::Publisher<tf2_msgs::msg::TFMessage>::SharedPtr pub_;
};

/// A `/tf` transform published by a separate participant is written into the
/// arena, and reads back through `tft_plan_at` with the pose that was sent.
///
/// This is the whole of what the ROS half contributes to an applied transform:
/// the QoS that lets it arrive, and the field-by-field unpack into
/// `tft_bridge_sample`.
///
/// **Mutant:** in `BridgeHandle::offer_one`, swap the `s.pose[0]` and
/// `s.pose[3]` assignments — write `rotation.z` into the `qw` slot and
/// `rotation.w` into `qz`. That is `geometry_msgs`' `x y z w` order colliding
/// with `docs/PHASE1.md` §3.1's `qw qx qy qz`, it produces a *valid unit
/// quaternion* so nothing upstream refuses it, and this test then reads back
/// `qw = 0.2588` against the expected `0.9659` and fails. Applied; it dies.
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

/// A transform for an edge the config does not declare is dropped and counted
/// (§5.8's amendment), and the bridge keeps ingesting the ones it does declare.
///
/// The two halves matter together: an implementation that stopped on the first
/// undeclared edge would pass an assertion on `dropped_undeclared` alone.
///
/// **Mutant:** in `BridgeHandle::ingest`, `break` out of the transform loop
/// instead of continuing after the first sample. `dropped_undeclared` still
/// reaches 1 — the undeclared transform is first in the message — but nothing
/// is ever applied and the `applied >= 1` expectation fails. Applied; it dies.
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
  // §5.9's ledger. It balances on a healthy bridge *and* on this one, which is
  // the only interesting case: a path that returns without counting is exactly
  // how "we are not dropping anything" becomes false with no test failing.
  EXPECT_EQ(
    stats.applied + stats.rejected_by_arena + stats.static_verified + stats.dropped_authority +
    stats.dropped_non_monotonic + stats.dropped_bad_name + stats.dropped_kind_change +
    stats.dropped_undeclared + stats.dropped_bad_pose + stats.refused_after_halt,
    stats.transforms);
}

/// A config the engine will not build fails in the constructor, not at the
/// first message (§5.5 is NORMATIVE about the domain case, and the same
/// argument covers every startup refusal).
///
/// **Mutant:** in `BridgeHandle::run`, call `ready.set_value(TFT_OK)` instead
/// of `ready.set_value(rc)`. The constructor then returns normally with a NULL
/// `bridge_`, this test's `EXPECT_THROW` fails — and every later offer would
/// have dereferenced NULL on the ingest thread. Applied; it dies.
TEST_F(IngestTest, a_config_that_does_not_parse_throws_from_the_constructor)
{
  tf_tree_ros::BridgeOptions o = options();
  o.topology_toml = "[[edge]]\nparent = \"a\"\n";  // no child, no kind
  EXPECT_THROW(tf_tree_ros::BridgeHandle(node_.get(), o), tf_tree_ros::BridgeError);
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
