// `docs/PHASE4.md` §5.3 and §5.4: publisher attribution. The Rust half is in
// `crates/tf_tree_bridge`; here, that `publisher_gid` equals `endpoint_gid()`.

#include <atomic>
#include <chrono>
#include <memory>
#include <string>
#include <thread>

#include <gtest/gtest.h>

#include <rclcpp/rclcpp.hpp>
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

/// One increasing stamp source, 10 ms apart (inside §5.5's reset threshold).
int64_t next_stamp()
{
  static std::atomic<int64_t> stamp{1'000'000'000};
  return stamp.fetch_add(10'000'000);
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

/// Publishes one transform repeatedly: a single publish into an undiscovered
/// subscription is lost.
class Broadcaster
{
public:
  Broadcaster(const std::string & name, const std::string & topic)
  : node_(std::make_shared<rclcpp::Node>(name)),
    pub_(node_->create_publisher<tf2_msgs::msg::TFMessage>(
        topic, rclcpp::QoS(rclcpp::KeepLast(100)).reliable()))
  {
  }

  /// Stamps come from one shared clock so only §5.4's authority can drop.
  void publish_once() {pub_->publish(message_at(next_stamp()));}

  std::string qualified_name() const
  {
    std::string ns = node_->get_namespace();
    if (ns.empty() || ns.back() != '/') {
      ns += '/';
    }
    return ns + node_->get_name();
  }

private:
  rclcpp::Node::SharedPtr node_;
  rclcpp::Publisher<tf2_msgs::msg::TFMessage>::SharedPtr pub_;
};

tf_tree_ros::BridgeOptions options_on(const std::string & topic)
{
  tf_tree_ros::BridgeOptions o;
  o.topology_toml = kTopology;
  o.tf_topic = topic;
  o.tf_static_topic = topic + "_static";
  return o;
}

/// `FirstWriterWins` drops the second publisher on an edge and the diagnostic
/// names both nodes and the edge (§5.4, §6.3).
TEST(Attribution, a_second_publisher_on_one_edge_is_dropped_and_both_nodes_are_named)
{
  const std::string topic = "/tf_authority";

  auto node = std::make_shared<rclcpp::Node>("attribution_bridge");
  tf_tree_ros::BridgeHandle bridge(node.get(), options_on(topic));

  Broadcaster owner("authoritative_odom", topic);
  ASSERT_TRUE(
    wait_for(
      [&] {
        owner.publish_once();
        return bridge.stats().applied >= 1;
      },
      20s)) << "the first publisher never reached the arena";

  // The second appears only now, so the winner is determined.
  Broadcaster intruder("impostor_ekf", topic);
  ASSERT_TRUE(
    wait_for(
      [&] {
        intruder.publish_once();
        return bridge.stats().dropped_authority >= 1;
      },
      20s)) << "the second publisher was never dropped by authority: either the GIDs did not "
               "resolve to two distinct nodes, or FirstWriterWins is not being applied. applied="
            << bridge.stats().applied
            << " dropped_authority=" << bridge.stats().dropped_authority
            << " dropped_non_monotonic=" << bridge.stats().dropped_non_monotonic
            << " dropped_undeclared=" << bridge.stats().dropped_undeclared;

  // An early conflict may name `<unknown publisher>` (§5.3); wait for it to settle.
  const std::string want_owner = owner.qualified_name();
  const std::string want_intruder = intruder.qualified_name();
  ASSERT_TRUE(
    wait_for(
      [&] {
        intruder.publish_once();
        const auto c = bridge.last_authority_conflict();
        return c.observed && c.owner == want_owner && c.intruder == want_intruder;
      },
      20s));

  const auto conflict = bridge.last_authority_conflict();
  EXPECT_EQ(conflict.owner, want_owner);
  EXPECT_EQ(conflict.intruder, want_intruder);
  EXPECT_EQ(conflict.parent, "odom");
  EXPECT_EQ(conflict.child, "base_link");
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
