// `docs/PHASE4.md` §5.8 forms 1 and 2: the parameter surface, every parameter
// that can silently do nothing. Component registration is checked by `ros/build.sh`.

#include <chrono>
#include <memory>
#include <stdexcept>
#include <string>
#include <thread>

#include <gtest/gtest.h>

#include <rclcpp/rclcpp.hpp>
#include <tf2_msgs/msg/tf_message.hpp>

#include "tf_tree_ros/bridge_node.hpp"

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

rclcpp::NodeOptions with(const std::vector<rclcpp::Parameter> & params)
{
  rclcpp::NodeOptions o;
  o.parameter_overrides(params);
  return o;
}

tf2_msgs::msg::TFMessage message_at(int64_t stamp_ns)
{
  geometry_msgs::msg::TransformStamped t;
  t.header.frame_id = "odom";
  t.child_frame_id = "base_link";
  t.header.stamp.sec = static_cast<int32_t>(stamp_ns / 1000000000LL);
  t.header.stamp.nanosec = static_cast<uint32_t>(stamp_ns % 1000000000LL);
  t.transform.rotation.w = 1.0;

  tf2_msgs::msg::TFMessage msg;
  msg.transforms.push_back(t);
  return msg;
}

/// Topic parameters fail invisibly: a bridge on `/tf` hears nothing on a
/// namespaced stream.
TEST(BridgeNodeTest, the_topic_parameters_are_the_topics_the_bridge_subscribes_to)
{
  auto node = std::make_shared<tf_tree_ros::BridgeNode>(
    with(
      {
        rclcpp::Parameter("topology_config", std::string(kTopology)),
        rclcpp::Parameter("tf_topic", std::string("/tf_node_test")),
        rclcpp::Parameter("tf_static_topic", std::string("/tf_node_test_static")),
      }));

  auto publisher = std::make_shared<rclcpp::Node>("node_test_broadcaster");
  auto pub = publisher->create_publisher<tf2_msgs::msg::TFMessage>(
    "/tf_node_test", rclcpp::QoS(rclcpp::KeepLast(100)).reliable());

  int64_t stamp = 1'000'000'000;
  const auto deadline = std::chrono::steady_clock::now() + 20s;
  while (std::chrono::steady_clock::now() < deadline) {
    stamp += 10'000'000;
    pub->publish(message_at(stamp));
    if (node->bridge().stats().applied >= 1) {
      break;
    }
    std::this_thread::sleep_for(20ms);
  }

  EXPECT_GE(node->bridge().stats().applied, 1u);
}

/// No topology is refused (§5.8's amendment, `docs/decisions/0004`); the empty
/// topology itself is refused in `tft_bridge_create`.
TEST(BridgeNodeTest, a_node_given_no_topology_at_all_refuses_to_start)
{
  EXPECT_THROW(
    std::make_shared<tf_tree_ros::BridgeNode>(rclcpp::NodeOptions()), std::invalid_argument);
}

/// Both topology parameters set is refused. The file is `/dev/null` so
/// `read_file`'s own refusal cannot pass this vacuously.
TEST(BridgeNodeTest, a_node_given_two_topologies_refuses_to_start)
{
  EXPECT_THROW(
    std::make_shared<tf_tree_ros::BridgeNode>(
      with(
        {
          rclcpp::Parameter("topology_config", std::string(kTopology)),
          rclcpp::Parameter("topology_config_file", std::string("/dev/null")),
        })),
    std::invalid_argument);
}

/// A misspelled `authority` is refused rather than defaulted (§5.4).
TEST(BridgeNodeTest, an_unknown_authority_policy_is_refused_rather_than_defaulted)
{
  EXPECT_THROW(
    std::make_shared<tf_tree_ros::BridgeNode>(
      with(
        {
          rclcpp::Parameter("topology_config", std::string(kTopology)),
          rclcpp::Parameter("authority", std::string("first_writer_win")),
        })),
    std::invalid_argument);
}

/// An `arena_name` with invisible whitespace is refused (`""` vs `"  "`, `" spaced"`
/// vs `"spaced"`); other malformed names are the ABI's to refuse.
TEST(BridgeNodeTest, an_unseeable_whitespace_arena_name_is_refused_rather_than_published)
{
  for (const std::string & name : {std::string("  "), std::string(" spaced")}) {
    EXPECT_THROW(
      std::make_shared<tf_tree_ros::BridgeNode>(
        with(
          {
            rclcpp::Parameter("topology_config", std::string(kTopology)),
            rclcpp::Parameter("arena_name", name),
          })),
      std::invalid_argument) << "arena_name=\"" << name << "\" was not refused";
  }
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
