// `docs/PHASE4.md` §5.8 forms 1 and 2 — the node that wraps form 3.
//
// The node adds parameters and nothing else. Every decision about an offered
// transform is still `tf_tree_bridge`'s, one boundary further down; every
// decision about subscriptions and QoS is still `BridgeHandle`'s. What lives
// here is the translation from ROS parameters into `BridgeOptions`, which is
// the only thing forms 1 and 2 have that form 3 does not.

#ifndef TF_TREE_ROS__BRIDGE_NODE_HPP_
#define TF_TREE_ROS__BRIDGE_NODE_HPP_

#include <memory>

#include <rclcpp/rclcpp.hpp>

#include "tf_tree_ros/bridge_handle.hpp"

namespace tf_tree_ros
{

/// A standalone ingest-bridge node.
///
/// `RCLCPP_COMPONENTS_REGISTER_NODE` in `bridge_component.cpp` turns this into
/// **both** §5.8 form 2 (a loadable component) and §5.8 form 1 (a standalone
/// executable, `tf_tree_bridge`) from one registration.
///
/// # Parameters
///
/// | name | type | default | meaning |
/// |---|---|---|---|
/// | `topology_config_file` | string | `""` | path to the file `tf_tree topology --discover` writes |
/// | `topology_config` | string | `""` | the same content inline, for a launch file that would rather not ship a file |
/// | `tf_prefix` | string | `""` | §5.6, applied to the wire **and** to the declared topology |
/// | `authority` | string | `first_writer_wins` | §5.4: also `last_writer_wins`, `strict` |
/// | `on_clock_reset` | string | `halt` | §5.5: also `recreate` |
/// | `time_domain` | int | `0` | §5.5's domain tag; every declared dynamic edge must agree |
/// | `queue_depth` | int | `100` | §5.2's `KeepLast` depth on both topics |
/// | `tf_topic` / `tf_static_topic` | string | `/tf`, `/tf_static` | for a namespaced or replayed stream |
/// | `arena_name` | string | `""` | `docs/decisions/0015`: publish the arena under this rendezvous name so a **separate process** can attach; empty is a private in-process arena |
///
/// `arena_name` is the only parameter here that all three of §5.8's deployment
/// forms do not reach the same way. Forms 1 and 2 are this node, so they get it
/// from the parameter like every row above; **form 3 sets
/// `BridgeOptions::arena_name` directly**, because it never constructs a
/// `BridgeNode` and so has no parameters at all. The field is the surface; this
/// parameter is one way of filling it.
///
/// Exactly one of `topology_config_file` and `topology_config` must be set. The
/// engine has no runtime edge declaration (§5.8's amendment, `docs/decisions/0004`,
/// D4), so a bridge with no topology has nothing it could ever write and the
/// constructor refuses rather than starting a node that drops everything.
class BridgeNode : public rclcpp::Node
{
public:
  explicit BridgeNode(const rclcpp::NodeOptions & options);

  /// The ingest bridge this node owns — its stats, its remap table, and its
  /// `tft_tree` handle for readers in the same process.
  const BridgeHandle & bridge() const noexcept {return *bridge_;}

private:
  std::unique_ptr<BridgeHandle> bridge_;
};

}  // namespace tf_tree_ros

#endif  // TF_TREE_ROS__BRIDGE_NODE_HPP_
