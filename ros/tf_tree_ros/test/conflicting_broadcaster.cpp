// One broadcaster process, for `docs/PHASE4.md` §9's multi-publisher launch
// fixture — the box that asks for detection "verified against a deliberately
// broken launch file".
//
// # Why a separate process at all
//
// `test_attribution.cpp` already constructs two publishers on one edge and
// asserts that the second is dropped and both nodes are named. It does it
// **in one process**, which is a different claim: two `rclcpp::Node`s in one
// process share a participant, and on most RMWs they do not exercise
// cross-process discovery at all. §5.3's whole mechanism is matching
// `rmw_message_info_t::publisher_gid` against `TopicEndpointInfo::endpoint_gid()`
// across the graph, and the interesting failure — the GIDs not agreeing — is a
// property of the middleware between *processes*. This executable is the half
// that test cannot supply.
//
// # Two things this gets wrong if it is written the obvious way
//
// **The stamp has to come from a clock the other broadcaster shares.**
// `test_attribution.cpp` carries the scar: its first version gave each
// broadcaster its own counter starting at a fixed base, and the second one's
// samples were refused as **non-monotonic** (§5.5) before the authority table
// ever saw them — so the test failed, for a reason that had nothing to do with
// authority. Two nodes in one process can share a `static` counter; two
// processes cannot, so this uses the node's own clock, which both processes
// read from the same source. `use_sim_time` is left alone deliberately: the
// fixture runs on the real clock and the bridge is configured to match.
//
// **A publisher that starts and immediately publishes may not be attributable
// yet.** The bridge resolves a GID to a node name by walking
// `get_publishers_info_by_topic`, and an endpoint can lag its own first sample.
// The §5.4 diagnostic fires on `first_time` per edge, so a conflict caught
// inside that window is logged as `<unattributed>` **and never logged again**.
// The owner's case is worse: `authority.rs:246` clones the publisher into the
// owner slot on its first accepted sample and never updates it, so an owner
// that published before the graph listed it stays unattributed for the life of
// the bridge. `start_delay_s` therefore exists for **both** broadcasters — it
// is the difference between a fixture that tests §5.4 and one that tests
// discovery latency.

#include <chrono>
#include <cstdint>
#include <memory>
#include <string>

#include "geometry_msgs/msg/transform_stamped.hpp"
#include "rclcpp/rclcpp.hpp"
#include "tf2_msgs/msg/tf_message.hpp"

int main(int argc, char ** argv)
{
  rclcpp::init(argc, argv);

  auto node = std::make_shared<rclcpp::Node>("conflicting_broadcaster");

  const auto parent = node->declare_parameter<std::string>("parent", "odom");
  const auto child = node->declare_parameter<std::string>("child", "base_link");
  const auto topic = node->declare_parameter<std::string>("tf_topic", "/tf");
  const auto rate_hz = node->declare_parameter<double>("rate_hz", 50.0);
  const auto start_delay_s = node->declare_parameter<double>("start_delay_s", 0.0);

  if (rate_hz <= 0.0) {
    RCLCPP_FATAL(node->get_logger(), "rate_hz must be positive");
    return 2;
  }

  // §5.2's `/tf` QoS, which is also `tf2_ros`'s: reliable, volatile,
  // `KeepLast`. A mismatch here would be dropped by the middleware before the
  // bridge saw it, and the fixture would pass its own no-op.
  auto pub = node->create_publisher<tf2_msgs::msg::TFMessage>(
    topic, rclcpp::QoS(rclcpp::KeepLast(100)).reliable());

  // See the header: discovery, not politeness.
  if (start_delay_s > 0.0) {
    rclcpp::sleep_for(
      std::chrono::nanoseconds(static_cast<int64_t>(start_delay_s * 1e9)));
  }

  const auto period =
    std::chrono::nanoseconds(static_cast<int64_t>(1e9 / rate_hz));
  // `create_wall_timer`, and captured by value: the callback outlives this
  // scope's bindings in every way that matters once `spin` is entered, and a
  // reference capture here is the kind of thing that works until somebody adds
  // an early return.
  auto timer = node->create_wall_timer(
    period,
    [node, pub, parent, child]() {
      geometry_msgs::msg::TransformStamped t;
      t.header.frame_id = parent;
      t.child_frame_id = child;
      // The shared clock. Both processes read the same source, so the two
      // streams interleave monotonically and the only reason to drop one is
      // §5.4's authority — which is the thing under test.
      t.header.stamp = node->now();
      t.transform.rotation.w = 1.0;
      tf2_msgs::msg::TFMessage m;
      m.transforms.push_back(t);
      pub->publish(m);
    });

  rclcpp::spin(node);
  rclcpp::shutdown();
  return 0;
}
