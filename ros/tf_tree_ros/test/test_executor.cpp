// `docs/PHASE4.md` §5.8's amendment: form 3 attaches to a node the caller already
// owns and spins. The flag `create_callback_group(MutuallyExclusive,
// /*automatically_add_to_executor_with_node=*/false)` keeps every offer on the
// bridge's own thread (else `TFT_ERR_WRONG_THREAD`, or an uncaught
// `std::runtime_error` from `add_callback_group`). No other test spins the
// node it hands to `BridgeHandle`, so this is the only gate on that flag.

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

/// A node already in the caller's executor and spinning, with the bridge
/// attached afterwards (§5.8 form 3's usage). The order matters: attaching first
/// survives the mutation, since `add_node` skips a group another executor owns.
///
/// Mutant: make `create_callback_group(MutuallyExclusive, false)` `true`; the
/// ingest thread's `add_callback_group` throws uncaught and the binary crashes.
TEST(ExecutorTest, a_bridge_attaches_to_a_node_the_caller_is_already_spinning)
{
  auto node = std::make_shared<rclcpp::Node>("already_spinning_host");

  // The caller's executor holds the node and runs *before* the bridge exists.
  rclcpp::executors::SingleThreadedExecutor caller_exec;
  caller_exec.add_node(node);
  std::atomic<bool> stop{false};
  std::thread caller_thread([&caller_exec, &stop] {
      while (!stop.load()) {
        caller_exec.spin_once(50ms);
      }
    });

  // Let it actually be spinning.
  std::this_thread::sleep_for(200ms);

  uint64_t applied = 0;
  {
    tf_tree_ros::BridgeOptions o;
    o.topology_toml = kTopology;
    o.tf_topic = "/tf_executor_test";
    o.tf_static_topic = "/tf_executor_test_static";
    tf_tree_ros::BridgeHandle bridge(node.get(), o);

    auto publisher = std::make_shared<rclcpp::Node>("executor_test_broadcaster");
    auto pub = publisher->create_publisher<tf2_msgs::msg::TFMessage>(
      "/tf_executor_test", rclcpp::QoS(rclcpp::KeepLast(100)).reliable());

    int64_t stamp = 1'000'000'000;
    const auto deadline = std::chrono::steady_clock::now() + 20s;
    while (std::chrono::steady_clock::now() < deadline) {
      stamp += 10'000'000;
      pub->publish(message_at(stamp));
      std::this_thread::sleep_for(20ms);
      applied = bridge.stats().applied;
      if (applied >= 1) {
        break;
      }
    }
  }

  stop.store(true);
  caller_thread.join();
  caller_exec.remove_node(node);

  EXPECT_GE(applied, 1u)
    << "the bridge's subscriptions never fired while the caller spun the node: either the "
       "callback group was taken by the caller's executor, or it was taken by neither";
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
