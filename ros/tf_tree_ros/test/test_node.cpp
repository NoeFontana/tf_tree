// `docs/PHASE4.md` §5.8 forms 1 and 2 — the parameter surface, which is the
// only thing they add to form 3.
//
// The component machinery itself is rclcpp's to test; that a
// `RCLCPP_COMPONENTS_REGISTER_NODE` produces a plugin and an executable is
// checked as an artifact by `ros/build.sh`. What is this package's to get
// wrong, and what is here, is every parameter that can silently do nothing.

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

/// The topic parameters are the ones that fail *invisibly*: a bridge that
/// ignored `tf_topic` would subscribe to `/tf`, receive nothing on a namespaced
/// or replayed stream, and report a perfectly healthy zero.
///
/// **Mutant:** in `BridgeNode`'s constructor, replace the `tf_topic` parameter
/// read with the literal `"/tf"`. The bridge then listens on `/tf` while this
/// test publishes on `/tf_node_test`, nothing is ever applied, and the wait
/// times out. Applied; it dies.
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

/// A node with no topology declares nothing, so it can never write anything —
/// §5.8's amendment, `docs/decisions/0004`, D4.
///
/// **This check is the only thing in the stack that refuses an empty topology.**
/// `tft_bridge_create("")` returns `TFT_OK`: an empty config is a legal config
/// describing a tree with no edges. So the failure mode without it is not an
/// error at all — the node starts, logs "ingest bridge up", and reports every
/// transform on the robot as `TFT_BRIDGE_UNDECLARED`.
///
/// **Mutant:** delete the `throw` in the both-or-neither check. Construction
/// then succeeds and `EXPECT_THROW` reports "it throws nothing". Applied; it
/// dies — and the docstring's first version, which predicted a `BridgeError`
/// from a refusing `tft_bridge_create`, was wrong about *why*, which is how the
/// paragraph above came to be measured.
TEST(BridgeNodeTest, a_node_given_no_topology_at_all_refuses_to_start)
{
  EXPECT_THROW(
    std::make_shared<tf_tree_ros::BridgeNode>(rclcpp::NodeOptions()), std::invalid_argument);
}

/// Both topology parameters set is equally refused: there is no rule for which
/// one wins that an operator could predict.
///
/// The file is `/dev/null` on purpose. A path that does not exist would make
/// this test pass through `read_file`'s own refusal even with the check gone —
/// a test that holds for a reason other than the one it is named for.
///
/// **Mutant:** delete the `throw` in the both-or-neither check. `/dev/null`
/// reads as an empty config, which `tft_bridge_create` accepts, so nothing
/// throws and the test fails. Applied; it dies.
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

/// A misspelled `authority` is refused rather than defaulted.
///
/// §5.4 documents `last_writer_wins` as chaotic and `strict` as the CI policy,
/// so a typo quietly becoming `first_writer_wins` is an operator believing the
/// bridge enforces something it does not.
///
/// **Mutant:** make `parse_authority` return `Authority::FirstWriterWins` for an
/// unknown string instead of throwing. Nothing throws and `EXPECT_THROW` fails.
/// Applied; it dies.
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

}  // namespace

int main(int argc, char ** argv)
{
  ::testing::InitGoogleTest(&argc, argv);
  rclcpp::init(argc, argv);
  const int rc = RUN_ALL_TESTS();
  rclcpp::shutdown();
  return rc;
}
