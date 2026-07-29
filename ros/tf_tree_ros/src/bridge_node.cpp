// `docs/PHASE4.md` §5.8 forms 1 and 2 — ROS parameters into `BridgeOptions`.

#include "tf_tree_ros/bridge_node.hpp"

#include <fstream>
#include <sstream>
#include <stdexcept>
#include <string>
#include <utility>

namespace tf_tree_ros
{
namespace
{

Authority parse_authority(const std::string & s)
{
  if (s == "first_writer_wins") {
    return Authority::FirstWriterWins;
  }
  if (s == "last_writer_wins") {
    return Authority::LastWriterWins;
  }
  if (s == "strict") {
    return Authority::Strict;
  }
  // Refusing an unknown value rather than falling back to the default: §5.4
  // documents `last_writer_wins` as chaotic and `strict` as the CI policy, and
  // a typo silently becoming `first_writer_wins` would be an operator believing
  // the bridge is enforcing something it is not.
  throw std::invalid_argument(
          "authority must be first_writer_wins, last_writer_wins or strict, not '" + s + "'");
}

OnClockReset parse_on_clock_reset(const std::string & s)
{
  if (s == "halt") {
    return OnClockReset::Halt;
  }
  if (s == "recreate") {
    return OnClockReset::Recreate;
  }
  throw std::invalid_argument("on_clock_reset must be halt or recreate, not '" + s + "'");
}

std::string read_file(const std::string & path)
{
  std::ifstream in(path);
  if (!in) {
    throw std::invalid_argument("topology_config_file: cannot open '" + path + "'");
  }
  std::ostringstream buffer;
  buffer << in.rdbuf();
  return buffer.str();
}

}  // namespace

BridgeNode::BridgeNode(const rclcpp::NodeOptions & options)
: rclcpp::Node("tf_tree_bridge", options)
{
  BridgeOptions o;

  const auto config_file = declare_parameter<std::string>("topology_config_file", "");
  const auto config_text = declare_parameter<std::string>("topology_config", "");
  if (config_file.empty() == config_text.empty()) {
    // Both or neither, and **this check is the only thing that refuses an empty
    // topology anywhere in the stack.** Measured: `tft_bridge_create("")`
    // returns `TFT_OK` — an empty config is a legal config describing a tree
    // with no edges — so without this a bridge with no `topology_config` starts
    // clean, logs "ingest bridge up", and reports every transform on the robot
    // as `TFT_BRIDGE_UNDECLARED`. That is the same shape as the `tf_prefix`
    // defect §5.6's clarification records: a switch that drops 100 % of the
    // traffic with nothing failing at startup.
    //
    // "Both" is refused too because no rule for which one wins is one an
    // operator could predict.
    throw std::invalid_argument(
            "set exactly one of topology_config_file and topology_config. Produce a config with "
            "`tf_tree topology --discover`; the engine cannot declare edges at run time "
            "(docs/PHASE4.md §5.8, docs/decisions/0004).");
  }
  o.topology_toml = config_file.empty() ? config_text : read_file(config_file);

  o.tf_prefix = declare_parameter<std::string>("tf_prefix", "");
  o.authority = parse_authority(declare_parameter<std::string>("authority", "first_writer_wins"));
  o.on_clock_reset =
    parse_on_clock_reset(declare_parameter<std::string>("on_clock_reset", "halt"));

  const auto domain = declare_parameter<int64_t>("time_domain", 0);
  if (domain < 0 || domain > 255) {
    throw std::invalid_argument("time_domain must be in 0..=255");
  }
  o.time_domain = static_cast<uint8_t>(domain);

  const auto depth = declare_parameter<int64_t>("queue_depth", 100);
  if (depth < 1) {
    throw std::invalid_argument("queue_depth must be at least 1");
  }
  o.queue_depth = static_cast<size_t>(depth);

  o.tf_topic = declare_parameter<std::string>("tf_topic", "/tf");
  o.tf_static_topic = declare_parameter<std::string>("tf_static_topic", "/tf_static");

  // §5.5, as far as a node can take it. The engine's typed domains are what
  // actually keep sim and real transforms apart, and the C ABI refuses at
  // startup if a declared edge disagrees with `time_domain` — so all that is
  // left here is to say so when an operator has asked for simulated time and
  // left the arena tagged with the same domain a real robot would use. Making
  // this an error instead would break the legitimate case of a config whose
  // edges all declare a sim domain explicitly.
  if (get_parameter("use_sim_time").as_bool() && o.time_domain == 0) {
    RCLCPP_WARN(
      get_logger(),
      "use_sim_time is true but time_domain is 0, the same tag a real-time bridge uses. A "
      "consumer querying this arena cannot be told the difference; give the simulated tree its "
      "own domain (docs/PHASE4.md §5.5).");
  }

  bridge_ = std::make_unique<BridgeHandle>(this, std::move(o));
}

}  // namespace tf_tree_ros
