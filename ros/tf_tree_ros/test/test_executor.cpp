// `docs/PHASE4.md` §5.8's amendment — the property the amendment exists to
// establish, exercised the way §5.8 describes form 3 being used.
//
// The amendment's whole argument is one flag:
//
//     create_callback_group(MutuallyExclusive,
//                           /*automatically_add_to_executor_with_node=*/false)
//
// `tft_bridge` is `Send + !Sync` and its affinity is checked, so every callback
// that offers a transform must run on the thread that created the bridge. Left
// at the default, the node's own executor claims the group and a caller who
// spins the node gets `TFT_ERR_WRONG_THREAD` on every transform — or, in the
// ordering a real deployment uses, a `std::runtime_error` out of
// `add_callback_group` on the ingest thread, which is uncaught there and takes
// the process with it.
//
// **No other test in this package spins the node it hands to `BridgeHandle`**,
// so before this file the flag could be flipped and all fourteen results stayed
// green. That is the gap: form 3's headline property — "attach to an existing
// node you already own and spin" — was asserted nowhere.

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

/// **A node already in the caller's executor and already spinning, with the
/// bridge attached afterwards.** This is §5.8 form 3's advertised usage — "a
/// team that already has a node they can edit" — and the ordering matters:
///
/// * Attaching the bridge *first* and calling `add_node` after **survives** the
///   mutation below, because rclcpp's `add_node` silently skips a callback
///   group another executor already owns. A test written that way asserts
///   nothing.
/// * Attaching to a node the caller's executor has already taken is the
///   ordering a real integration produces, and it is the one that fails.
///
/// **Mutant:** in `BridgeHandle`'s constructor, change
/// `create_callback_group(MutuallyExclusive, false)` to `true`. The caller's
/// executor claims the group when `add_node` runs, `exec_->add_callback_group`
/// on the ingest thread then throws `std::runtime_error`, and nothing catches it
/// on that thread — the process dies with `terminate called after throwing an
/// instance of 'std::runtime_error'` and this binary reports a crash rather than
/// a failure. Applied; it dies. (A crash is a harsher death than an assertion,
/// and it is the honest one: that is what the flag being wrong actually does to
/// a node built this way.)
TEST(ExecutorTest, a_bridge_attaches_to_a_node_the_caller_is_already_spinning)
{
  auto node = std::make_shared<rclcpp::Node>("already_spinning_host");

  // The caller's executor, holding the node and running, *before* the bridge
  // exists. Nothing about this is unusual — it is what a node with its own
  // timers and subscriptions looks like at the moment somebody adds ingest.
  rclcpp::executors::SingleThreadedExecutor caller_exec;
  caller_exec.add_node(node);
  std::atomic<bool> stop{false};
  std::thread caller_thread([&caller_exec, &stop] {
      while (!stop.load()) {
        caller_exec.spin_once(50ms);
      }
    });

  // Give the caller's executor a moment to actually be spinning rather than
  // merely constructed, so this is the ordering it claims to be.
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
