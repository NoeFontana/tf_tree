// `docs/PHASE4.md` §6.3's QoS regression test, as amended. A volatile
// `/tf_static` subscription (§5.2's "most common ROS 2 tf integration bug")
// breaks nothing any other test looks at. Every test uses its own topic: a
// `transient_local` writer's history outlives its test, so a shared name would
// make results order-dependent.

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

/// The static edge is declared with the constant the broadcaster publishes, so an
/// arrival is `STATIC_VERIFIED` (§5.7, §5.8) and a corrupted one would land in
/// `static_conflicts`.
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
  // §5.7: a static's stamp never touches the clock (often zero).
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

/// A latched static broadcaster: `KeepLast(100)`, reliable, `transient_local`, as
/// `tf2_ros::StaticTransformBroadcaster`.
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
  // A `/tf` topic per test too, so strays cannot move these counters.
  o.tf_topic = static_topic + "_dynamic";
  return o;
}

/// Poll `predicate` until it holds or `timeout` elapses.
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

/// A bare `volatile` subscriber on the same topic: §6.3's negative control.
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

/// §6.3, amended: a static broadcaster publishes once and stays alive; the bridge
/// starts afterwards and must receive it, and a `volatile` subscriber must not.
/// The control makes the bridge's receipt mean something.
///
/// Mutant A: `qos_static` `.transient_local()` -> `.durability_volatile()` in
/// `BridgeHandle`'s constructor; the `static_verified` wait times out.
/// Mutant B: give `VolatileControl` `.transient_local()`; `control.received()`
/// is no longer 0.
TEST(QosRegression, a_late_joiner_receives_a_latched_static_and_a_volatile_subscriber_does_not)
{
  const std::string topic = "/tf_static_live";

  auto broadcaster_node = std::make_shared<rclcpp::Node>("latched_static_broadcaster");
  auto broadcaster = make_broadcaster(broadcaster_node, topic);
  broadcaster->publish(static_message());

  // Both late joiners get their own participants: sharing the broadcaster's is
  // an easier delivery path than a real deployment's.
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

/// §6.3's originally-specified test, which **must not pass**: `TRANSIENT_LOCAL`
/// is publisher-lifetime-scoped, so once the broadcaster is gone its sample is
/// too. Pinning it keeps the amendment falsifiable. The sample dies with the
/// *writer*, not the participant: `broadcaster_node.reset()` alone is inert
/// (the publisher holds the node's interfaces).
///
/// Mutant: delete `broadcaster.reset()`; the bridge then receives the latched
/// sample and `EXPECT_FALSE(got)` fails.
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

/// §5.2's NORMATIVE table, asserted against what the middleware negotiated
/// (`get_actual_qos()`), not the constructor's literals. Reliability has no
/// observable consequence a test can wait for (`best_effort` still matches a
/// reliable publisher), so it needs this direct check; `queue_depth` is asserted
/// on a non-default value since nothing else sets it.
///
/// Mutants: `qos_tf` `.reliable()` -> `.best_effort()`; `qos_static`
/// `.transient_local()` -> `.durability_volatile()`; `KeepLast(100)` in place of
/// `KeepLast(opts_.queue_depth)`. Each fails here.
TEST(QosRegression, the_negotiated_qos_is_the_one_5_2_is_normative_about)
{
  auto node = std::make_shared<rclcpp::Node>("qos_reader");
  tf_tree_ros::BridgeOptions o = options_on("/tf_static_negotiated");
  o.queue_depth = 37;
  tf_tree_ros::BridgeHandle bridge(node.get(), o);

  EXPECT_EQ(bridge.actual_tf_qos().reliability(), rclcpp::ReliabilityPolicy::Reliable);
  EXPECT_EQ(bridge.actual_tf_qos().durability(), rclcpp::DurabilityPolicy::Volatile);
  EXPECT_EQ(bridge.actual_tf_qos().depth(), 37u);

  EXPECT_EQ(bridge.actual_tf_static_qos().reliability(), rclcpp::ReliabilityPolicy::Reliable);
  EXPECT_EQ(
    bridge.actual_tf_static_qos().durability(), rclcpp::DurabilityPolicy::TransientLocal);
  EXPECT_EQ(bridge.actual_tf_static_qos().depth(), 37u);
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
