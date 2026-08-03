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
/// What this asserts is the *parameter* surface: "neither parameter set" is
/// indistinguishable from an empty string by the time it reaches C, and this is
/// where an operator gets told which parameter to set. The empty topology
/// itself is refused one layer down, in `tft_bridge_create`, so all three of
/// §5.8's deployment forms inherit the refusal —
/// `IngestTest.form_3_refuses_a_topology_that_declares_no_edges` is the same
/// property asserted against form 3.
///
/// **Mutant:** delete the `throw` in the both-or-neither check. Construction
/// reaches `tft_bridge_create("")`, which now refuses it, so a `BridgeError`
/// comes out instead — a different type from the `std::invalid_argument` this
/// expects, and `EXPECT_THROW` reports the mismatch. Applied; it dies.
/// (This test's first docstring predicted exactly that `BridgeError` and was
/// wrong at the time, because `tft_bridge_create("")` then returned `TFT_OK`.
/// It is true now because that was fixed, not because the prediction was.)
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
/// reads as an empty config, `config_file` wins the ternary below it, and
/// `tft_bridge_create` refuses that with a `BridgeError` — not the
/// `std::invalid_argument` this expects, so `EXPECT_THROW` fails on the type.
/// Applied; it dies.
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

/// An `arena_name` that is entirely whitespace is refused rather than published.
///
/// This is the *only* content rule this layer applies to `arena_name`, and the
/// reason is the one `BridgeNode`'s constructor gives at the parameter: empty
/// means "no shared arena", so `""` and `" "` look identical in a launch file
/// and mean opposite things, and by the time `" "` reaches C it is an ordinary
/// valid single-component name that `tf_tree_ipc::ArenaName` accepts. Every
/// other malformed name — too long, `../escape`, a NUL — is the ABI's to refuse
/// and it does, with the name in the message.
///
/// **Mutant:** delete the whitespace check in `BridgeNode`'s constructor. `" "`
/// then reaches `tft_bridge_create`, which accepts it as a name, and the node
/// constructs — nothing throws and `EXPECT_THROW` fails. Applied; it dies.
/// (Should the surrounding environment make the *arena* unbuildable, the throw
/// becomes a `BridgeError` instead, which `EXPECT_THROW` also reports as a
/// failure because the type does not match. The test cannot pass with the check
/// gone.)
TEST(BridgeNodeTest, an_all_whitespace_arena_name_is_refused_rather_than_published)
{
  EXPECT_THROW(
    std::make_shared<tf_tree_ros::BridgeNode>(
      with(
        {
          rclcpp::Parameter("topology_config", std::string(kTopology)),
          rclcpp::Parameter("arena_name", std::string("  ")),
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
