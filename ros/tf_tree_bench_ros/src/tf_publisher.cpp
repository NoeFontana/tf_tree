// The one publisher every arm of the §9.1 comparison listens to.
//
// **One publisher, not one per arm**, and that is the point: `docs/PHASE5.md`
// §9.3 is normative that both stacks run "on the same data" with "identical
// QoS, identical executor configuration, identical DDS vendor and version". The
// cheapest way to guarantee that is to have exactly one process producing the
// traffic and to run the arms against it, so there is no second publisher whose
// configuration could quietly differ.
//
// QoS is `docs/PHASE4.md` §5.2's, which is also `tf2_ros`'s: `/tf` is reliable,
// volatile, `KeepLast(depth)`; `/tf_static` is reliable, **transient_local**, so
// a consumer that joins late still receives it. A volatile `/tf_static` is the
// classic silent failure — the late joiner simply never learns the static half
// of the tree and every lookup through it fails forever.
//
// The plan file is written by `dds_report emit-config`, from the same
// `tf_tree_bench::workload` catalogue the Rust harnesses use, so a DDS row and a
// `contended_scaling` row on the same workload name describe the same tree.

#include <chrono>
#include <cmath>
#include <cstdint>
#include <fstream>
#include <map>
#include <memory>
#include <sstream>
#include <string>
#include <vector>

#include "geometry_msgs/msg/transform_stamped.hpp"
#include "rclcpp/rclcpp.hpp"
#include "tf2_msgs/msg/tf_message.hpp"

namespace
{

struct DynEdge
{
  std::string parent;
  std::string child;
  double rate_hz;
};

struct StaticEdge
{
  std::string parent;
  std::string child;
  double q[4];   // wxyz
  double t[3];
};

struct Plan
{
  std::vector<DynEdge> dynamic;
  std::vector<StaticEdge> statics;
};

Plan read_plan(const std::string & path)
{
  std::ifstream f(path);
  if (!f) {
    throw std::runtime_error("cannot read plan file " + path);
  }
  Plan p;
  std::string line;
  while (std::getline(f, line)) {
    if (line.empty() || line[0] == '#') {continue;}
    std::istringstream s(line);
    std::string kind;
    s >> kind;
    if (kind == "D") {
      DynEdge e;
      s >> e.parent >> e.child >> e.rate_hz;
      p.dynamic.push_back(e);
    } else if (kind == "S") {
      StaticEdge e;
      s >> e.parent >> e.child >> e.q[0] >> e.q[1] >> e.q[2] >> e.q[3] >>
        e.t[0] >> e.t[1] >> e.t[2];
      p.statics.push_back(e);
    }
  }
  return p;
}

/// A smooth, bounded pose for edge `seed` at time `t`.
///
/// It does not have to match the Rust fixture's trajectory — no arm compares
/// values against a reference here, only against each other, and both arms
/// receive the identical bytes from this one publisher. What it does have to be
/// is *varying*, so neither engine's interpolation is handed two identical
/// samples to bracket between.
void pose_at(double seed, double t, double q[4], double xyz[3])
{
  const double a = 0.2 * std::sin(0.7 * t + seed);
  q[0] = std::cos(a);
  q[1] = 0.0;
  q[2] = 0.0;
  q[3] = std::sin(a);
  xyz[0] = 0.3 * std::cos(0.6 * t + 0.5 * seed);
  xyz[1] = 0.2 * std::sin(0.4 * t + 0.2 * seed);
  xyz[2] = 0.05 * std::sin(1.1 * t + 0.7 * seed);
}

class Publisher : public rclcpp::Node
{
public:
  Publisher(const Plan & plan, double seconds)
  : rclcpp::Node("tf_bench_publisher"), plan_(plan)
  {
    // §5.2 / tf2_ros defaults. `KeepLast(100)` on /tf matches the bridge's
    // `queue_depth` default, so neither side is given a deeper queue than the
    // other.
    tf_ = create_publisher<tf2_msgs::msg::TFMessage>("/tf", rclcpp::QoS(rclcpp::KeepLast(100)));
    tf_static_ = create_publisher<tf2_msgs::msg::TFMessage>(
      "/tf_static", rclcpp::QoS(rclcpp::KeepLast(1)).transient_local());

    publish_statics();

    // One timer per distinct rate, each publishing every dynamic edge at that
    // rate in ONE `TFMessage`. That is what a real broadcaster does — a
    // `robot_state_publisher` sends its whole joint set per tick — and it also
    // keeps the message count identical for both arms.
    std::map<int64_t, std::vector<size_t>> by_period_us;
    for (size_t i = 0; i < plan_.dynamic.size(); ++i) {
      const auto us = static_cast<int64_t>(1e6 / plan_.dynamic[i].rate_hz);
      by_period_us[us].push_back(i);
    }
    for (const auto & [us, edges] : by_period_us) {
      timers_.push_back(
        create_wall_timer(
          std::chrono::microseconds(us),
          [this, edges]() {this->tick(edges);}));
    }

    RCLCPP_INFO(
      get_logger(), "publishing %zu dynamic edges over %zu rate groups, %zu statics, for %.0f s",
      plan_.dynamic.size(), by_period_us.size(), plan_.statics.size(), seconds);

    stop_ = create_wall_timer(
      std::chrono::milliseconds(static_cast<int64_t>(seconds * 1000.0)),
      [this]() {
        RCLCPP_INFO(get_logger(), "published %lu messages", messages_);
        rclcpp::shutdown();
      });
  }

private:
  void publish_statics()
  {
    tf2_msgs::msg::TFMessage m;
    const auto stamp = now();
    for (const auto & e : plan_.statics) {
      geometry_msgs::msg::TransformStamped ts;
      ts.header.stamp = stamp;
      ts.header.frame_id = e.parent;
      ts.child_frame_id = e.child;
      ts.transform.rotation.w = e.q[0];
      ts.transform.rotation.x = e.q[1];
      ts.transform.rotation.y = e.q[2];
      ts.transform.rotation.z = e.q[3];
      ts.transform.translation.x = e.t[0];
      ts.transform.translation.y = e.t[1];
      ts.transform.translation.z = e.t[2];
      m.transforms.push_back(ts);
    }
    if (!m.transforms.empty()) {
      tf_static_->publish(m);
    }
  }

  void tick(const std::vector<size_t> & edges)
  {
    tf2_msgs::msg::TFMessage m;
    const auto stamp = now();
    const double t = static_cast<double>(stamp.nanoseconds()) * 1e-9;
    m.transforms.reserve(edges.size());
    for (size_t i : edges) {
      const auto & e = plan_.dynamic[i];
      double q[4];
      double xyz[3];
      pose_at(static_cast<double>(i), t, q, xyz);
      geometry_msgs::msg::TransformStamped ts;
      ts.header.stamp = stamp;
      ts.header.frame_id = e.parent;
      ts.child_frame_id = e.child;
      ts.transform.rotation.w = q[0];
      ts.transform.rotation.x = q[1];
      ts.transform.rotation.y = q[2];
      ts.transform.rotation.z = q[3];
      ts.transform.translation.x = xyz[0];
      ts.transform.translation.y = xyz[1];
      ts.transform.translation.z = xyz[2];
      m.transforms.push_back(ts);
    }
    tf_->publish(m);
    ++messages_;
  }

  Plan plan_;
  rclcpp::Publisher<tf2_msgs::msg::TFMessage>::SharedPtr tf_;
  rclcpp::Publisher<tf2_msgs::msg::TFMessage>::SharedPtr tf_static_;
  std::vector<rclcpp::TimerBase::SharedPtr> timers_;
  rclcpp::TimerBase::SharedPtr stop_;
  uint64_t messages_ = 0;
};

}  // namespace

int main(int argc, char ** argv)
{
  rclcpp::init(argc, argv);

  std::string plan_path;
  double seconds = 30.0;
  for (int i = 1; i < argc; ++i) {
    const std::string a = argv[i];
    if (a == "--plan" && i + 1 < argc) {
      plan_path = argv[++i];
    } else if (a == "--seconds" && i + 1 < argc) {
      seconds = std::stod(argv[++i]);
    }
  }
  if (plan_path.empty()) {
    fprintf(stderr, "usage: tf_publisher --plan <file> [--seconds N]\n");
    return 2;
  }

  try {
    auto node = std::make_shared<Publisher>(read_plan(plan_path), seconds);
    rclcpp::spin(node);
  } catch (const std::exception & e) {
    fprintf(stderr, "tf_publisher: %s\n", e.what());
    return 1;
  }
  rclcpp::shutdown();
  return 0;
}
