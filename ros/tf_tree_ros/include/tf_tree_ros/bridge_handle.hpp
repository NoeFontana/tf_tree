// tf_tree ROS 2 ingest bridge — `docs/PHASE4.md` §5.8 deployment form 3.
//
// **This file makes no ingest decisions.** Authority (§5.4), clock resets
// (§5.5), name normalization (§5.6), static semantics (§5.7) and every counter
// (§5.9) live in `tf_tree_bridge`, behind `tft_bridge_offer`. What is here is
// the half that needs a middleware: subscribe with the right QoS, unpack
// `tf2_msgs/TFMessage` into the POD sample, offer it, and turn the returned
// outcome into a log line. If you find yourself writing an `if` about who owns
// an edge, that decision already exists in Rust.

#ifndef TF_TREE_ROS__BRIDGE_HANDLE_HPP_
#define TF_TREE_ROS__BRIDGE_HANDLE_HPP_

#include <atomic>
#include <cstdint>
#include <future>
#include <mutex>
#include <stdexcept>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include <geometry_msgs/msg/transform_stamped.hpp>
#include <rclcpp/rclcpp.hpp>
#include <tf2_msgs/msg/tf_message.hpp>

// Two opt-ins, both deliberate, and both belong here rather than in the
// CMakeLists so a reader of this file sees what it is accepting:
//
//   * `TFT_ENABLE_UNSTABLE` — §3.1's unstable tier has no stability guarantee
//     and the header refuses to compile without an explicit acknowledgement.
//     The whole bridge surface is in that tier.
//   * `TFT_HAVE_BRIDGE` — the bridge declarations are additionally behind the
//     default-off `bridge` cargo feature, so both halves of the switch have to
//     be on: without this the declarations are hidden, and without
//     `cargo build --features bridge` the symbols are not in the archive.
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

/// How the bridge is configured. Everything here is decided before the first
/// message; nothing in this struct is consulted per sample.
struct BridgeOptions
{
  /// The topology config, as **text** (§5.8's amendment). The ABI takes text
  /// rather than a path because a ROS node's topology arrives as a parameter,
  /// a launch argument or a bag sidecar, all of which are already strings.
  ///
  /// `tf_tree topology --discover` writes this file.
  std::string topology_toml;

  /// §5.6's `tf_prefix`, or empty for none. It rewrites the *declared* names
  /// as well as the wire, so a config produced by `--discover` on robot 1 can
  /// be reused verbatim for robot 2.
  std::string tf_prefix;

  Authority authority = Authority::FirstWriterWins;
  OnClockReset on_clock_reset = OnClockReset::Halt;

  /// The time-domain tag the arena is stamped in (§5.5). Every declared
  /// dynamic edge must agree with it or construction fails — at startup, by
  /// design, rather than at the first message.
  uint8_t time_domain = 0;

  /// §5.2's `KeepLast` depth, on both topics. 100 is `tf2_ros`' value and the
  /// one §5.2 is NORMATIVE about; it is settable so a bag-replay harness can
  /// widen it, not so a deployment can narrow it.
  size_t queue_depth = 100;

  std::string tf_topic = "/tf";
  std::string tf_static_topic = "/tf_static";
};

/// A `tft_status` that a `tf_tree_ros` call could not proceed past.
///
/// Construction failures are the only ones that throw: a per-sample rejection
/// is a *log line*, not an exception, because the bridge's whole job is to keep
/// running while it reports somebody else's misconfigured robot (§5.3, §5.4).
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
/// # It owns a thread, and that is not an optimisation
///
/// `tft_bridge` is `Send + !Sync` and its affinity is checked: the thread that
/// calls `tft_bridge_create` is the only one that may offer, read stats, or
/// free. §5.9 independently asks for a dedicated `SingleThreadedExecutor` on
/// its own thread. Those two requirements have one implementation, so this
/// class *is* that thread — it creates the bridge on it, adds its own callback
/// group to an executor running on it, and frees the bridge on it.
///
/// The alternative — handing the caller a callback group to add to their
/// executor — was rejected because it cannot be made correct: the bridge would
/// have been created on the constructing thread and every offer would then
/// arrive from the executor's, which the ABI refuses with
/// `TFT_ERR_WRONG_THREAD`. §5.8's "all three share one implementation; only the
/// lifecycle wrapper differs" is therefore amended: a dedicated callback group
/// is design, not a wrapper.
///
/// # Lifetime
///
/// The node must outlive the handle. The destructor stops the executor, joins
/// the thread and frees the bridge; it does not throw.
class BridgeHandle
{
public:
  /// Create the arena the config declares, claim every dynamic edge in it, and
  /// start ingesting.
  ///
  /// Throws `BridgeError` if the config does not parse, declares a domain the
  /// bridge does not stamp in (§5.5), or names an edge another participant
  /// already owns. The node is left untouched in that case.
  BridgeHandle(rclcpp::Node * node, BridgeOptions options);

  ~BridgeHandle();

  BridgeHandle(const BridgeHandle &) = delete;
  BridgeHandle & operator=(const BridgeHandle &) = delete;

  /// A `tft_tree` handle onto the arena this bridge writes, for reading.
  ///
  /// Owned by the handle and valid until it is destroyed. Unlike the bridge
  /// itself this is `Send + Sync`, so any thread may plan and sample through
  /// it while the ingest thread writes — that is Phase 1's whole design.
  tft_tree * tree() const noexcept {return tree_;}

  /// §5.9's counters, as of the last message ingested.
  ///
  /// **A snapshot, not a live read.** `tft_bridge_get_stats` is subject to the
  /// same thread affinity as every other call on the handle, so a diagnostic
  /// timer on the node's own executor cannot call it. The ingest thread
  /// therefore refreshes this copy after each `TFMessage` and this returns the
  /// copy — one ~120-byte struct copy per message, against a per-message cost
  /// already measured in hundreds of nanoseconds per *transform*.
  tft_bridge_stats stats() const;

  /// The callback group both subscriptions are on. Exposed for tests and
  /// diagnostics; the handle's own executor already drives it, and adding it
  /// to a second executor is an error rclcpp will report.
  rclcpp::CallbackGroup::SharedPtr callback_group() const noexcept {return group_;}

  /// §5.6's remap table, `from` (the name on the wire) to `to` (the name the
  /// arena declares). Complete before the first message, and logged by the
  /// constructor.
  const std::vector<std::pair<std::string, std::string>> & remap() const noexcept
  {
    return remap_;
  }

private:
  void run(std::promise<tft_status> & ready);
  tft_status create_bridge();
  void ingest(
    const tf2_msgs::msg::TFMessage & msg, const rclcpp::MessageInfo & info,
    tft_bridge_topic topic);
  void offer_one(
    const geometry_msgs::msg::TransformStamped & t, const uint8_t * gid,
    tft_bridge_topic topic);
  void report(const tft_bridge_outcome & out, tft_bridge_topic topic);
  void refresh_stats();

  rclcpp::Node * node_;
  BridgeOptions opts_;
  rclcpp::Logger log_;

  rclcpp::CallbackGroup::SharedPtr group_;
  rclcpp::Subscription<tf2_msgs::msg::TFMessage>::SharedPtr sub_tf_;
  rclcpp::Subscription<tf2_msgs::msg::TFMessage>::SharedPtr sub_static_;

  /// Only ever touched from `thread_`.
  tft_bridge * bridge_ = nullptr;
  std::thread thread_;
  std::atomic<bool> stop_{false};
  rclcpp::executors::SingleThreadedExecutor::SharedPtr exec_;

  /// `Send + Sync`, so it escapes the ingest thread. Freed by the destructor.
  tft_tree * tree_ = nullptr;

  mutable std::mutex stats_mutex_;
  tft_bridge_stats stats_{};

  std::vector<std::pair<std::string, std::string>> remap_;

  /// Written by the ingest thread before it publishes a failing status through
  /// the constructor's future, and read by the constructor after — which is
  /// what orders the two accesses.
  std::string create_error_;
};

}  // namespace tf_tree_ros

#endif  // TF_TREE_ROS__BRIDGE_HANDLE_HPP_
