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
  // Refuse, never fall back: a typo must not silently become `first_writer_wins`.
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
    // Both or neither: the ABI cannot tell either from an empty string. The
    // empty-topology refusal itself lives in `tft_bridge_create`.
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

  o.arena_name = declare_parameter<std::string>("arena_name", "");
  // The ABI judges whether a name is usable (`TFT_ERR_ARENA_UNAVAILABLE`); only
  // what it cannot see is refused here: an all-whitespace or space-padded name,
  // which reads like `""` or `"foo"` in a launch file but is a different
  // rendezvous (`docs/decisions/0015`). Refused, not trimmed.
  if (!o.arena_name.empty() &&
    (o.arena_name.find_first_not_of(" \t\n\r\f\v") != 0 ||
    o.arena_name.find_last_not_of(" \t\n\r\f\v") != o.arena_name.size() - 1))
  {
    throw std::invalid_argument(
            "arena_name has leading or trailing whitespace, or is entirely whitespace. A "
            "consumer selects the arena by exact name, so \" foo\" and \"foo\" are different "
            "rendezvous that read the same in a launch file. Leave it unset for a private "
            "in-process arena, or give it a name a consumer can put in $TF_TREE_NAME "
            "(docs/decisions/0015).");
  }

  // §5.5: the C ABI refuses a domain disagreement at startup; here only warn
  // when sim time is on but the tag is the real-time one.
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
