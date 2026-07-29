// `docs/PHASE4.md` §6.3's QoS regression test, as amended.
//
// §5.2 calls a `volatile` subscription to `/tf_static` "the single most common
// ROS 2 tf integration bug", and it presents as "my static transforms are
// missing" with no error anywhere. A regression to it would break nothing that
// any other test in this repository looks at, so this file is the only thing
// standing between the bridge and that bug.
//
// # Why every test here uses its own topic
//
// All three exercise `/tf_static` semantics in one process, and a
// `transient_local` writer's history outlives the test that created it by
// however long the participant takes to go away. Sharing one topic name would
// make each test's result depend on the order gtest happened to run them in.
// The topic name is a `BridgeOptions` field precisely so a harness can do this;
// §5.2's subject is the durability, not the string.

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

/// The static edge is declared with the constant the broadcaster publishes, so
/// a transform that arrives is `TFT_BRIDGE_STATIC_VERIFIED` (§5.7 idempotent,
/// §5.8 verification) and `static_verified` is the counter that moves. A value
/// that arrived *corrupted* would land in `static_conflicts` instead, so this
/// distinguishes "received" from "received and correct".
constexpr const char * kTopology = R"(
[[edge]]
parent = "base_link"
child = "lidar"
kind = "static"
pose = [0.9659258262890683, 0.0, 0.0, 0.25881904510252074, 0.35, -0.02, 0.61]
)";

tf2_msgs::msg::TFMessage static_message()
{
  geometry_msgs::msg::TransformStamped t;
  t.header.frame_id = "base_link";
  t.child_frame_id = "lidar";
  // §5.7: a static's stamp is meaningless and never touches the clock —
  // `robot_state_publisher` commonly stamps statics with zero.
  t.header.stamp.sec = 0;
  t.header.stamp.nanosec = 0;
  t.transform.rotation.w = 0.9659258262890683;
  t.transform.rotation.x = 0.0;
  t.transform.rotation.y = 0.0;
  t.transform.rotation.z = 0.25881904510252074;
  t.transform.translation.x = 0.35;
  t.transform.translation.y = -0.02;
  t.transform.translation.z = 0.61;

  tf2_msgs::msg::TFMessage msg;
  msg.transforms.push_back(t);
  return msg;
}

/// A latched static broadcaster: `KeepLast(100)`, reliable, `transient_local`,
/// exactly what `tf2_ros::StaticTransformBroadcaster` uses.
rclcpp::Publisher<tf2_msgs::msg::TFMessage>::SharedPtr make_broadcaster(
  const rclcpp::Node::SharedPtr & node, const std::string & topic)
{
  return node->create_publisher<tf2_msgs::msg::TFMessage>(
    topic, rclcpp::QoS(rclcpp::KeepLast(100)).reliable().transient_local());
}

tf_tree_ros::BridgeOptions options_on(const std::string & static_topic)
{
  tf_tree_ros::BridgeOptions o;
  o.topology_toml = kTopology;
  o.tf_static_topic = static_topic;
  // A `/tf` topic per test too, so a stray dynamic publisher from elsewhere in
  // the suite cannot move this bridge's counters.
  o.tf_topic = static_topic + "_dynamic";
  return o;
}

/// Poll `predicate` until it holds or `timeout` elapses. Returns whether it
/// held. A fixed sleep would be either flaky or slow; this is neither.
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

/// A bare `volatile` subscriber on the same topic, spun on its own executor —
/// §6.3's negative control.
class VolatileControl
{
public:
  VolatileControl(const std::string & name, const std::string & topic)
  : node_(std::make_shared<rclcpp::Node>(name))
  {
    sub_ = node_->create_subscription<tf2_msgs::msg::TFMessage>(
      topic, rclcpp::QoS(rclcpp::KeepLast(100)).reliable().durability_volatile(),
      [this](tf2_msgs::msg::TFMessage::ConstSharedPtr) {received_++;});
    exec_.add_node(node_);
    thread_ = std::thread([this] {
        while (!stop_.load()) {
          exec_.spin_once(50ms);
        }
      });
  }

  ~VolatileControl()
  {
    stop_.store(true);
    thread_.join();
    exec_.remove_node(node_);
  }

  uint64_t received() const {return received_.load();}

private:
  rclcpp::Node::SharedPtr node_;
  rclcpp::Subscription<tf2_msgs::msg::TFMessage>::SharedPtr sub_;
  rclcpp::executors::SingleThreadedExecutor exec_;
  std::thread thread_;
  std::atomic<bool> stop_{false};
  std::atomic<uint64_t> received_{0};
};

/// §6.3, amended: **a static broadcaster publishes once, stays alive, and never
/// publishes again; the bridge starts afterwards and must receive the
/// transform — and a `volatile` subscriber in the same run must not.**
///
/// The negative control is not decoration. Without it the test passes even if
/// durability is ignored entirely, provided the broadcaster happens to still be
/// publishing — and the bug §5.2 is about is invisible in exactly that case.
///
/// **Mutant A (the positive half):** in `BridgeHandle`'s constructor, change
/// `qos_static` from `.transient_local()` to `.durability_volatile()`. That is
/// the regression this file exists for, it produces no error anywhere, and the
/// `static_verified` wait then times out. Applied; it dies.
///
/// **Mutant B (the negative half):** give `VolatileControl` `.transient_local()`
/// instead of `.durability_volatile()`. The control then receives the latched
/// sample and `EXPECT_EQ(control.received(), 0u)` fails — which is what makes
/// the control's silence evidence rather than an accident of timing. Applied;
/// it dies.
TEST(QosRegression, a_late_joiner_receives_a_latched_static_and_a_volatile_subscriber_does_not)
{
  const std::string topic = "/tf_static_live";

  auto broadcaster_node = std::make_shared<rclcpp::Node>("latched_static_broadcaster");
  auto broadcaster = make_broadcaster(broadcaster_node, topic);
  broadcaster->publish(static_message());

  // Both late joiners in their own participants, created after the one and only
  // publication, and given the same wall-clock window to receive it. A separate
  // node matters: a reader sharing the broadcaster's participant is a different
  // and easier delivery path than the one a real deployment uses.
  VolatileControl control("volatile_control", topic);
  auto reader_node = std::make_shared<rclcpp::Node>("late_bridge_reader");
  tf_tree_ros::BridgeHandle bridge(reader_node.get(), options_on(topic));

  const bool got = wait_for(
    [&bridge] {return bridge.stats().static_verified >= 1;}, 15s);

  EXPECT_TRUE(got) << "the transient_local subscription received nothing from a live, silent "
                      "broadcaster — this is the /tf_static volatile regression";
  EXPECT_EQ(bridge.stats().static_conflicts, 0u)
    << "the transform arrived but its value did not match the declared constant";
  EXPECT_EQ(control.received(), 0u)
    << "a volatile subscriber received a latched sample published before it existed; "
       "the negative control is not controlling anything";
}

/// The corrected mental model behind §6.3's amendment: **`TRANSIENT_LOCAL` is
/// publisher-lifetime-scoped.** The sample is retained by the *writer*, so once
/// the broadcaster is gone the sample is gone with it, and no QoS setting on
/// the subscriber side recovers it.
///
/// This is §6.3's originally-specified test, and it is here as the thing that
/// **must not pass**: a correct bridge receives nothing. Pinning it keeps the
/// amendment's argument falsifiable rather than merely written down — if a
/// future RMW retained the sample, the amendment would be wrong and this is
/// what would say so.
///
/// **Measured here, and sharper than the amendment says: the retained sample
/// dies with the *writer*, not with the participant.** Destroying the node and
/// keeping the publisher leaves the sample being served — an `rclcpp::Publisher`
/// holds the node's interfaces alive, so `broadcaster_node.reset()` on its own
/// is inert. It is kept below because "the broadcaster exited" means both go,
/// and a reader should not have to rediscover that only one of them counts.
///
/// **Mutant:** delete the `broadcaster.reset()` line, keeping the writer alive
/// while the node goes away. The bridge then receives the latched sample and
/// `EXPECT_FALSE(got)` fails. Applied; it dies. (The mutant this docstring
/// first named — deleting `broadcaster_node.reset()` instead — was applied and
/// **survived**, which is how the paragraph above came to be measured rather
/// than assumed.)
TEST(QosRegression, a_broadcaster_that_has_exited_takes_its_static_transforms_with_it)
{
  const std::string topic = "/tf_static_gone";

  auto broadcaster_node = std::make_shared<rclcpp::Node>("departing_static_broadcaster");
  auto broadcaster = make_broadcaster(broadcaster_node, topic);
  broadcaster->publish(static_message());
  broadcaster.reset();
  broadcaster_node.reset();

  auto reader_node = std::make_shared<rclcpp::Node>("late_reader");
  tf_tree_ros::BridgeHandle bridge(reader_node.get(), options_on(topic));

  const bool got = wait_for(
    [&bridge] {return bridge.stats().transforms >= 1;}, 3s);

  EXPECT_FALSE(got)
    << "a departed broadcaster's TRANSIENT_LOCAL sample survived it. §6.3's amendment — "
       "and the four-way table it rests on — is wrong for this RMW, and the specified-but-"
       "unpassable form of this test would now pass.";
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
