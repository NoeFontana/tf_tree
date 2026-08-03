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
    // Both or neither. This used to be the **only** thing in the stack that
    // refused an empty topology, which left §5.8's form 3 — a `BridgeHandle`
    // constructed directly with `topology_toml = ""` — starting clean and
    // answering `TFT_BRIDGE_UNDECLARED` to 100 % of the robot's traffic. The
    // policy now lives in `tft_bridge_create`, where every other startup
    // refusal (domain, cycle, claim) already lives and where all three
    // deployment forms inherit it.
    //
    // What is left here is the *parameter* surface, which the ABI cannot see:
    // "neither parameter set" and "both set" are indistinguishable from an
    // empty string by the time they reach C, and "both" has no rule for which
    // one wins that an operator could predict. A caller that removes this still
    // gets a `BridgeError` from below rather than a healthy-looking bridge —
    // this one is the better message, not the safety net.
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
  // **One refusal here, and deliberately only one** — the same split the
  // both-or-neither check above states: what stays at the parameter surface is
  // what the ABI *cannot see*.
  //
  // The ABI is the single authority on whether a string is a usable arena name.
  // `tf_tree_ipc::ArenaName` refuses an over-64-byte or multi-component name,
  // `tft_bridge_create` reports that as `TFT_ERR_ARENA_UNAVAILABLE` with the
  // name in the message, and `BridgeHandle` turns it into a `BridgeError` out of
  // this constructor — so a bad name already refuses to start, with a better
  // diagnosis than a second, narrower rule here could give. (An *empty* name
  // never reaches `ArenaName` from this package at all: `BridgeHandle` maps `""`
  // to a NULL `arena_name`, which is the ABI's spelling for "private heap
  // arena".) Duplicating the ABI's rules would also make `tf_tree_ros` reject
  // names that `$TF_TREE_NAME`, `tf_tree serve` and the C ABI all accept, which
  // is one concept with two definitions and a config that works in one place and
  // not another.
  //
  // What the ABI cannot see is *whitespace an operator cannot see either*, and
  // there are two ways it bites. `arena_name: ""` and `arena_name: " "` are the
  // same string in a launch file and opposite instructions to the bridge: the
  // first is a private heap arena, the second is a published rendezvous named
  // " " that no consumer will guess. And `arena_name: " foo"` and
  // `arena_name: "foo"` are the same string to that same operator and *different
  // rendezvous* — `ArenaName` accepts both — so a consumer setting
  // `TF_TREE_NAME=foo` finds nothing, and the difference appears in no log line
  // on either side. Both are the "waiting on a rendezvous that will never
  // appear" failure `docs/decisions/0015` spends a paragraph on, reached by an
  // invisible character.
  //
  // **Refused, not trimmed.** This layer's job is to refuse what the ABI cannot
  // see, not to rewrite what the operator wrote: a silently trimmed name is a
  // config that means something other than what it says, and the next reader has
  // to know this rule exists to predict the rendezvous. So a name that is
  // entirely whitespace, or that has whitespace at either end, is refused here —
  // one condition, because it is one mistake — and every other name is the ABI's
  // to judge.
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
