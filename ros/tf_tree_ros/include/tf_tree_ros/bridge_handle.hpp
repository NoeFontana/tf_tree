// tf_tree ROS 2 ingest bridge (`docs/PHASE4.md` §5.8 form 3): the middleware
// half only; ingest decisions live behind `tft_bridge_offer`.

#ifndef TF_TREE_ROS__BRIDGE_HANDLE_HPP_
#define TF_TREE_ROS__BRIDGE_HANDLE_HPP_

#include <array>
#include <atomic>
#include <cstdint>
#include <future>
#include <map>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include <geometry_msgs/msg/transform_stamped.hpp>
#include <rcl/time.h>
#include <rclcpp/rclcpp.hpp>
#include <tf2_msgs/msg/tf_message.hpp>

// `TFT_ENABLE_UNSTABLE` (§3.1) and `TFT_HAVE_BRIDGE` (`bridge` cargo feature).
#define TFT_ENABLE_UNSTABLE 1
#define TFT_HAVE_BRIDGE 1
#include <tf_tree.h>
#include <tf_tree_unstable.h>

namespace tf_tree_ros
{

/// §5.4's authority policy, as a scoped enum over the ABI's codes.
enum class Authority : tft_bridge_authority
{
  FirstWriterWins = TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS,
  LastWriterWins = TFT_BRIDGE_AUTHORITY_LAST_WRITER_WINS,
  Strict = TFT_BRIDGE_AUTHORITY_STRICT,
};

/// §5.5's response to a backwards clock jump past the reset threshold.
enum class OnClockReset : tft_bridge_on_clock_reset
{
  Halt = TFT_BRIDGE_ON_CLOCK_RESET_HALT,
  Recreate = TFT_BRIDGE_ON_CLOCK_RESET_RECREATE,
};

/// Bridge configuration, fixed before the first message.
struct BridgeOptions
{
  /// The topology config as **text** (§5.8's amendment).
  std::string topology_toml;

  /// §5.6's `tf_prefix`, or empty; rewrites declared names as well as the wire.
  std::string tf_prefix;

  Authority authority = Authority::FirstWriterWins;
  OnClockReset on_clock_reset = OnClockReset::Halt;

  /// The time-domain tag (§5.5); every declared dynamic edge must agree.
  uint8_t time_domain = 0;

  /// §5.2's `KeepLast` depth on both topics; 100 is NORMATIVE.
  size_t queue_depth = 100;

  std::string tf_topic = "/tf";
  std::string tf_static_topic = "/tf_static";

  /// Rendezvous name for a **shared** arena, or empty for a private heap one
  /// (`docs/decisions/0015`, `0019` §3). Failure is a `BridgeError`; there is no
  /// heap fallback.
  std::string arena_name;
};

/// The most recent §5.4 authority conflict, with both publishers named.
struct AuthorityConflict
{
  /// False until a conflict has been seen.
  bool observed = false;
  /// The node that owns the edge.
  std::string owner;
  /// The node whose samples are being dropped.
  std::string intruder;
  std::string parent;
  std::string child;
};

/// A `tft_status` construction could not proceed past. Only construction throws.
class BridgeError : public std::runtime_error
{
public:
  BridgeError(tft_status status, const std::string & what)
  : std::runtime_error(what), status_(status) {}

  tft_status status() const noexcept {return status_;}

private:
  tft_status status_;
};

/// §5.8 form 3: an ingest bridge attached to a node the caller already owns.
///
/// `tft_bridge` has checked thread affinity, so this class creates, feeds and
/// frees it on its own thread and callback group (§5.9). The node must outlive
/// the handle; the destructor joins the thread and does not throw.
class BridgeHandle
{
public:
  /// Create the arena, claim every dynamic edge and start ingesting. Throws
  /// `BridgeError` on a config that does not parse, a domain mismatch (§5.5) or
  /// an edge another participant owns.
  BridgeHandle(rclcpp::Node * node, BridgeOptions options);

  ~BridgeHandle();

  BridgeHandle(const BridgeHandle &) = delete;
  BridgeHandle & operator=(const BridgeHandle &) = delete;

  /// A read handle onto the arena; valid until the handle is destroyed.
  tft_tree * tree() const noexcept {return tree_;}

  /// §5.9's counters as of the last message ingested (a snapshot).
  tft_bridge_stats stats() const;

  /// The callback group both subscriptions are on, for tests and diagnostics.
  rclcpp::CallbackGroup::SharedPtr callback_group() const noexcept {return group_;}

  /// The QoS the middleware gave the `/tf` subscription (§5.2's negotiated QoS).
  const rclcpp::QoS & actual_tf_qos() const noexcept {return actual_tf_qos_;}

  /// The QoS given the `/tf_static` subscription.
  const rclcpp::QoS & actual_tf_static_qos() const noexcept {return actual_tf_static_qos_;}

  /// §5.6's remap table, wire name to arena name.
  const std::vector<std::pair<std::string, std::string>> & remap() const noexcept
  {
    return remap_;
  }

  /// The most recent §5.4 authority conflict, or `{}`.
  AuthorityConflict last_authority_conflict() const;

private:
  using Gid = std::array<uint8_t, 16>;

  /// A time jump from rcl awaiting the ingest thread. The callback runs on the
  /// clock's thread and only writes here; `drain_time_jump` applies it. Held by
  /// `shared_ptr` and captured instead of `this`: rclcpp does not synchronize
  /// `~JumpHandler` against a running callback.
  struct JumpSlot
  {
    std::mutex mutex;
    /// The first jump wins.
    bool pending = false;
    /// `rcl_time_jump_t::delta` (new minus old), passed to the ABI unnegated.
    int64_t delta_nanos = 0;
    tft_bridge_jump_kind kind = 0;
    /// Jumps that arrived while one was pending.
    uint64_t coalesced = 0;
  };

  void run(std::promise<tft_status> & ready);
  tft_status create_bridge();
  void register_jump_callback();
  void drain_time_jump();
  void maybe_attribute(const uint8_t * gid);
  bool attribute_from_graph(const Gid & wanted);
  void ingest(
    const tf2_msgs::msg::TFMessage & msg, const rclcpp::MessageInfo & info,
    tft_bridge_topic topic);
  void offer_one(
    const geometry_msgs::msg::TransformStamped & t, const uint8_t * gid,
    tft_bridge_topic topic, int64_t received_steady_nanos);
  void report(const tft_bridge_outcome & out, tft_bridge_topic topic);
  void refresh_stats();

  rclcpp::Node * node_;
  BridgeOptions opts_;
  rclcpp::Logger log_;

  rclcpp::CallbackGroup::SharedPtr group_;
  rclcpp::Subscription<tf2_msgs::msg::TFMessage>::SharedPtr sub_tf_;
  rclcpp::Subscription<tf2_msgs::msg::TFMessage>::SharedPtr sub_static_;

  /// Read back in the constructor only; `rclcpp::QoS` has no default constructor.
  rclcpp::QoS actual_tf_qos_{rclcpp::KeepLast(1)};
  rclcpp::QoS actual_tf_static_qos_{rclcpp::KeepLast(1)};

  /// Per-GID state (§5.3): graph walks spent, or `kResolved`. Ingest thread only.
  std::map<Gid, uint32_t> gid_state_;

  /// Receipt clock for the step detector and `*_THROTTLE`. Not
  /// `node_->get_clock()`, which under `use_sim_time` is `/clock`. Ingest thread.
  rclcpp::Clock steady_{RCL_STEADY_TIME};

  /// Whether an arena refusal has been logged yet.
  bool rejected_reported_ = false;

  /// Only ever touched from `thread_`.
  tft_bridge * bridge_ = nullptr;
  std::thread thread_;
  std::atomic<bool> stop_{false};
  rclcpp::executors::SingleThreadedExecutor::SharedPtr exec_;

  /// `Send + Sync`; freed by the destructor.
  tft_tree * tree_ = nullptr;

  /// Guards both snapshots below. Held only for the copy.
  mutable std::mutex stats_mutex_;
  tft_bridge_stats stats_{};
  AuthorityConflict conflict_;

  std::vector<std::pair<std::string, std::string>> remap_;

  /// Written by the ingest thread before it fails the constructor's future.
  std::string create_error_;

  /// Hand-off from rcl's jump callback to the ingest thread. Never null.
  std::shared_ptr<JumpSlot> jump_slot_ = std::make_shared<JumpSlot>();

  /// Keeps the jump callback alive; null if registration was refused. Declared
  /// after `jump_slot_` so it is unregistered first.
  rclcpp::JumpHandler::SharedPtr jump_handler_;
};

}  // namespace tf_tree_ros

#endif  // TF_TREE_ROS__BRIDGE_HANDLE_HPP_
