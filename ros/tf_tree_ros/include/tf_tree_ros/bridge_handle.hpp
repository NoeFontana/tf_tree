// tf_tree ROS 2 ingest bridge — `docs/PHASE4.md` §5.8 deployment form 3.
//
// No ingest decisions live here: authority (§5.4), clock resets (§5.5), names
// (§5.6), statics (§5.7) and counters (§5.9) are `tf_tree_bridge`'s, behind
// `tft_bridge_offer`. This is the middleware half: subscribe with the right QoS,
// unpack `TFMessage`, offer, log the outcome.

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

// Two opt-ins: `TFT_ENABLE_UNSTABLE` (§3.1's unstable tier, which holds the whole
// bridge surface) and `TFT_HAVE_BRIDGE` (declarations are also behind the
// default-off `bridge` cargo feature).
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
  /// The topology config, as **text** (§5.8's amendment); `tf_tree topology
  /// --discover` writes it.
  std::string topology_toml;

  /// §5.6's `tf_prefix`, or empty. It rewrites the declared names as well as the
  /// wire.
  std::string tf_prefix;

  Authority authority = Authority::FirstWriterWins;
  OnClockReset on_clock_reset = OnClockReset::Halt;

  /// The time-domain tag the arena is stamped in (§5.5); every declared dynamic
  /// edge must agree or construction fails.
  uint8_t time_domain = 0;

  /// §5.2's `KeepLast` depth on both topics; 100 is NORMATIVE. Settable so a
  /// replay harness can widen it, not so a deployment can narrow it.
  size_t queue_depth = 100;

  std::string tf_topic = "/tf";
  std::string tf_static_topic = "/tf_static";

  /// Rendezvous name for a **shared** arena, or empty for a private heap one
  /// (`docs/decisions/0015`). Non-empty lets a separate process attach read-only
  /// with `tf_tree::open()`, `tft_tree_open()` or `tf_tree.open()`. It is not
  /// `time_domain`; the rendezvous domain is `$TF_TREE_DOMAIN`, else
  /// `$ROS_DOMAIN_ID`, else 0 (`docs/decisions/0019` §3). Failure (name held,
  /// runtime directory unusable, no `shm`) is a `BridgeError`; there is no
  /// fallback to a heap arena.
  std::string arena_name;
};

/// The most recent §5.4 authority conflict, with both publishers named, in a form
/// a program can read (the diagnostic itself goes to the log).
struct AuthorityConflict
{
  /// False until a conflict has been seen; the other fields are empty then.
  bool observed = false;
  /// The node that owns the edge, per §5.3's GID cache.
  std::string owner;
  /// The node whose samples are being dropped.
  std::string intruder;
  std::string parent;
  std::string child;
};

/// A `tft_status` construction could not proceed past. Only construction throws;
/// a per-sample rejection is a log line (§5.3, §5.4).
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
/// # Thread
///
/// `tft_bridge` is `Send + !Sync` with checked affinity (`TFT_ERR_WRONG_THREAD`),
/// and §5.9 asks for a dedicated `SingleThreadedExecutor`. This class is that
/// thread: it creates, feeds and frees the bridge on it, with its own callback
/// group. (Handing the caller a group for their executor cannot be made correct.)
///
/// # Lifetime
///
/// The node must outlive the handle. The destructor stops the executor, joins
/// the thread and frees the bridge; it does not throw.
class BridgeHandle
{
public:
  /// Create the arena the config declares, claim every dynamic edge and start
  /// ingesting. Throws `BridgeError` if the config does not parse, declares a
  /// domain the bridge does not stamp in (§5.5), or names an edge another
  /// participant owns; the node is left untouched.
  BridgeHandle(rclcpp::Node * node, BridgeOptions options);

  ~BridgeHandle();

  BridgeHandle(const BridgeHandle &) = delete;
  BridgeHandle & operator=(const BridgeHandle &) = delete;

  /// A `tft_tree` handle onto the arena for reading; valid until the handle is
  /// destroyed, and `Send + Sync`.
  tft_tree * tree() const noexcept {return tree_;}

  /// §5.9's counters as of the last message ingested: a snapshot the ingest
  /// thread refreshes per `TFMessage`, since `tft_bridge_get_stats` has thread
  /// affinity.
  tft_bridge_stats stats() const;

  /// The callback group both subscriptions are on, for tests and diagnostics.
  rclcpp::CallbackGroup::SharedPtr callback_group() const noexcept {return group_;}

  /// The QoS the middleware actually gave the `/tf` subscription (§5.2's
  /// "log the **negotiated** QoS"), so §5.2's NORMATIVE table is assertable:
  /// `best_effort` still matches a reliable publisher and regresses silently.
  const rclcpp::QoS & actual_tf_qos() const noexcept {return actual_tf_qos_;}

  /// The QoS actually given the `/tf_static` subscription; its durability is
  /// §5.2's most common integration bug.
  const rclcpp::QoS & actual_tf_static_qos() const noexcept {return actual_tf_static_qos_;}

  /// §5.6's remap table, `from` (wire name) to `to` (arena name); complete before
  /// the first message.
  const std::vector<std::pair<std::string, std::string>> & remap() const noexcept
  {
    return remap_;
  }

  /// The most recent §5.4 authority conflict, or `{}`; a snapshot like `stats()`.
  AuthorityConflict last_authority_conflict() const;

private:
  using Gid = std::array<uint8_t, 16>;

  /// One time jump reported by rcl, waiting for the ingest thread to apply it.
  /// rclcpp fires the callback from the clock's own thread, and every
  /// `tft_bridge_*` call is affinity-checked (debug aborts, release loses the
  /// jump), so the callback only writes here and `drain_time_jump` applies it.
  /// Held by `shared_ptr` and captured **instead of `this`**: rclcpp does not
  /// synchronize `~JumpHandler` against a running callback.
  struct JumpSlot
  {
    std::mutex mutex;
    /// False when there is nothing to apply. The first jump wins.
    bool pending = false;
    /// `rcl_time_jump_t::delta`: **the new time minus the old**, so a rewind is
    /// negative. Passed to the ABI unnegated.
    int64_t delta_nanos = 0;
    tft_bridge_jump_kind kind = 0;
    /// Jumps that arrived while one was pending; counted, not overwriting the
    /// first, which is the transition that matters.
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

  /// Read back in the constructor and never touched again; `rclcpp::QoS` has no
  /// default constructor.
  rclcpp::QoS actual_tf_qos_{rclcpp::KeepLast(1)};
  rclcpp::QoS actual_tf_static_qos_{rclcpp::KeepLast(1)};

  /// §5.3's per-GID state, touched only from the ingest thread: how many graph
  /// walks this GID has cost, or `kResolved` once one of them matched it.
  std::map<Gid, uint32_t> gid_state_;

  /// The only clock this class reads: the receipt clock for the step detector and
  /// the `*_THROTTLE` clock. Not `node_->get_clock()`: under `use_sim_time` that
  /// is `/clock`, which reads 0 at boot and rewinds with a looping bag,
  /// suppressing exactly the diagnostics needed then. `RCL_STEADY_TIME` must be
  /// named (the default is the system clock). Ingest-thread only.
  rclcpp::Clock steady_{RCL_STEADY_TIME};

  /// Whether an arena refusal has been logged yet (the first is unconditional).
  bool rejected_reported_ = false;

  /// Only ever touched from `thread_`.
  tft_bridge * bridge_ = nullptr;
  std::thread thread_;
  std::atomic<bool> stop_{false};
  rclcpp::executors::SingleThreadedExecutor::SharedPtr exec_;

  /// `Send + Sync`, so it escapes the ingest thread. Freed by the destructor.
  tft_tree * tree_ = nullptr;

  /// Guards both snapshots below. Held only for the copy.
  mutable std::mutex stats_mutex_;
  tft_bridge_stats stats_{};
  AuthorityConflict conflict_;

  std::vector<std::pair<std::string, std::string>> remap_;

  /// Written by the ingest thread before it fails the constructor's future.
  std::string create_error_;

  /// The hand-off from rcl's jump callback to the ingest thread. Never null.
  std::shared_ptr<JumpSlot> jump_slot_ = std::make_shared<JumpSlot>();

  /// Keeps the jump callback alive (rclcpp holds a `weak_ptr`); null when
  /// registration was refused, a degradation (§5.3's rule). Declared after
  /// `jump_slot_` so it is unregistered first; the destructor also resets it.
  rclcpp::JumpHandler::SharedPtr jump_handler_;
};

}  // namespace tf_tree_ros

#endif  // TF_TREE_ROS__BRIDGE_HANDLE_HPP_
