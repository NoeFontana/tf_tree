// `docs/PHASE4.md` §5.8 forms 1 and 2; translates ROS parameters into `BridgeOptions`.

#ifndef TF_TREE_ROS__BRIDGE_NODE_HPP_
#define TF_TREE_ROS__BRIDGE_NODE_HPP_

#include <memory>

#include <rclcpp/rclcpp.hpp>

#include "tf_tree_ros/bridge_handle.hpp"

namespace tf_tree_ros
{

/// A standalone ingest-bridge node: §5.8 form 2 (component) and form 1
/// (executable) via `bridge_component.cpp`.
///
/// # Parameters
///
/// | name | type | default | meaning |
/// |---|---|---|---|
/// | `topology_config_file` | string | `""` | path to the file `tf_tree topology --discover` writes |
/// | `topology_config` | string | `""` | the same content inline |
/// | `tf_prefix` | string | `""` | §5.6, applied to the wire **and** to the declared topology |
/// | `authority` | string | `first_writer_wins` | §5.4: also `last_writer_wins`, `strict` |
/// | `on_clock_reset` | string | `halt` | §5.5: also `recreate` |
/// | `time_domain` | int | `0` | §5.5's domain tag; every declared dynamic edge must agree |
/// | `queue_depth` | int | `100` | §5.2's `KeepLast` depth on both topics |
/// | `tf_topic` / `tf_static_topic` | string | `/tf`, `/tf_static` | for a namespaced or replayed stream |
/// | `arena_name` | string | `""` | `docs/decisions/0015`: shared rendezvous name; empty is private |
///
/// Exactly one of `topology_config_file` and `topology_config` must be set
/// (`docs/decisions/0004`).
class BridgeNode : public rclcpp::Node
{
public:
  explicit BridgeNode(const rclcpp::NodeOptions & options);

  /// The ingest bridge this node owns.
  const BridgeHandle & bridge() const noexcept {return *bridge_;}

private:
  std::unique_ptr<BridgeHandle> bridge_;
};

}  // namespace tf_tree_ros

#endif  // TF_TREE_ROS__BRIDGE_NODE_HPP_
