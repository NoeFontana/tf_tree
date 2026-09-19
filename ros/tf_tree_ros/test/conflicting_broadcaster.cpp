// One broadcaster process for `docs/PHASE4.md` §9's multi-publisher launch
// fixture (detection "verified against a deliberately broken launch file").
//
// `test_attribution.cpp` covers two publishers in one process, where nodes share
// a participant; §5.3's GID match across *processes* is what this supplies.
//
// The stamp comes from the node's clock, which both processes share, so a drop
// can only be §5.4's authority (`use_sim_time` is left alone). `start_delay_s`
// exists for both broadcasters: a sample published before the graph lists its
// endpoint is logged `<unattributed>` once and the owner slot never updates.

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

  // §5.2's `/tf` QoS; a mismatch would be dropped by the middleware and pass vacuously.
  auto pub = node->create_publisher<tf2_msgs::msg::TFMessage>(
    topic, rclcpp::QoS(rclcpp::KeepLast(100)).reliable());

  // See the header.
  if (start_delay_s > 0.0) {
    rclcpp::sleep_for(
      std::chrono::nanoseconds(static_cast<int64_t>(start_delay_s * 1e9)));
  }

  const auto period =
    std::chrono::nanoseconds(static_cast<int64_t>(1e9 / rate_hz));
  // Captured by value: the callback outlives this scope.
  auto timer = node->create_wall_timer(
    period,
    [node, pub, parent, child]() {
      geometry_msgs::msg::TransformStamped t;
      t.header.frame_id = parent;
      t.child_frame_id = child;
      // The shared clock: the only reason to drop is §5.4's authority.
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
