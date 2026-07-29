// `docs/PHASE4.md` §5.3 and §5.4 — publisher attribution, and the diagnostic
// it exists to make possible.
//
// §5.4: "Being able to say 'your `/ekf` and `/odom_node` have both been
// publishing `odom -> base_link` for eight months' is a better sales pitch than
// any latency number." That sentence needs two node names, `TFMessage` carries
// none, and the only thing that does is the middleware's per-publisher GID.
// Everything in this file is about turning that GID into those names.
//
// The Rust half — the GID cache and the authority table — is unit-tested in
// `crates/tf_tree_bridge` and `crates/tf_tree_c/tests/bridge.rs`. What cannot
// be tested there, and is tested here, is that `rmw_message_info_t::publisher_gid`
// and `TopicEndpointInfo::endpoint_gid()` are **the same 16 bytes** on a real
// RMW. If they are not, every lookup misses, every publisher is
// `<unknown publisher>`, and §5.4 collapses into "somebody and somebody else".

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

/// One monotonically increasing stamp source for every broadcaster in the
/// process. 10 ms apart, comfortably inside §5.5's 100 ms reset threshold.
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

/// A node that publishes one transform repeatedly until told to stop.
///
/// Repeating rather than publishing once: DDS discovery is not instant and a
/// single publish into an undiscovered subscription is simply lost, so a
/// one-shot version of this test would be measuring discovery latency.
class Broadcaster
{
public:
  Broadcaster(const std::string & name, const std::string & topic)
  : node_(std::make_shared<rclcpp::Node>(name)),
    pub_(node_->create_publisher<tf2_msgs::msg::TFMessage>(
        topic, rclcpp::QoS(rclcpp::KeepLast(100)).reliable()))
  {
  }

  /// Publish with a stamp from a clock **shared by every broadcaster in this
  /// test**, so the only reason to drop is §5.4's authority.
  ///
  /// A per-broadcaster stamp is what a first version did, and it made the test
  /// fail for the wrong reason: the second publisher started at the beginning
  /// of its own timeline, which by then was tens of milliseconds behind the
  /// arena's high-water mark, so its samples were refused as non-monotonic
  /// (§5.5) before the authority table ever saw them. Two real robot nodes
  /// share a clock; two `Broadcaster`s have to as well.
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

/// Two publishers on one edge: `FirstWriterWins` keeps the first, and the
/// diagnostic names **both** nodes and the edge (§5.4, §6.3).
///
/// This is one test rather than two because the second half is what makes the
/// first mean anything. A bridge whose GID lookup always missed would report
/// every publisher as `<unknown publisher>` — and would then see *one*
/// publisher, not two, so `dropped_authority` would stay at zero and the
/// conflict would never be detected at all. Attribution is not decoration on
/// §5.4; it is the input.
///
/// It is also what caught the timing rule in `BridgeHandle::maybe_attribute`:
/// with a *periodic* refresh instead of one keyed on an unseen GID, the owner
/// was attributed a second after it took ownership, so the record froze at
/// `<unknown publisher> and /impostor_ekf have both been publishing …` — which
/// is §5.4's headline diagnostic with the half that sells it missing.
///
/// **Mutant A:** in `BridgeHandle::attribute_from_graph`, `continue` immediately
/// before the `tft_bridge_attribute` call, so no GID is ever cached. Both
/// publishers become `<unknown publisher>`, which is one publisher as far as
/// §5.4 is concerned; `dropped_authority` never moves and the wait times out.
/// Applied; it dies.
///
/// **Mutant B:** in `BridgeHandle::report`, delete the `conflict_` assignment
/// in the `TFT_BRIDGE_REASON_NOT_THE_OWNER` branch. `dropped_authority` still
/// climbs, so the counter half passes, and `observed` stays false — the names
/// §5.4 is entirely about are gone with no counter noticing. Applied; it dies.
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

  // Only now does the second one appear, so which of them wins is determined
  // rather than raced.
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

  // The record settles once §5.3's 1 Hz cache has seen the new publisher; until
  // then the intruder is legitimately `<unknown publisher>`, which §5.3 calls a
  // sanctioned degradation rather than a failure.
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
