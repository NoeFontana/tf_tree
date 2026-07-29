// `docs/PHASE4.md` §5.8 forms 2 and 1, from one registration.
//
// `rclcpp_components_register_node()` in the CMakeLists produces **both** the
// loadable component (form 2) and a standalone executable named
// `tf_tree_bridge` (form 1) from this single macro. Two deployment forms, one
// translation unit, and nothing in either of them that form 3 does not already
// do — which is what §5.8 means by "all three share one implementation".
//
// **Form 2's zero-serialization claim needs an argument passed.** §5.8 says
// that a bridge composed alongside the tf broadcasters "sees `TFMessage`
// without serialization at all". That is true of rclcpp 32 — verified here,
// including for `transient_local` `/tf_static` and for a late-joining
// subscription, both of which have historically been exceptions — but
// intra-process communication is **off by default when loading a component**.
// It has to be asked for:
//
//     ros2 component load /ComponentManager tf_tree_ros tf_tree_ros::BridgeNode \
//         --extra-arguments use_intra_process_comms:=true
//
// Without it the composed form pays exactly the deserialization cost §5.9 says
// the bridge is the one component that still pays, and the reason for composing
// it is gone.

#include <rclcpp_components/register_node_macro.hpp>

#include "tf_tree_ros/bridge_node.hpp"

RCLCPP_COMPONENTS_REGISTER_NODE(tf_tree_ros::BridgeNode)
