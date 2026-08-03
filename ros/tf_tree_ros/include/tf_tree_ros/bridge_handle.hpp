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

  /// Rendezvous name for a **shared** arena, or empty for a private heap one
  /// (`docs/decisions/0015`).
  ///
  /// Empty is the default and is exactly today's behaviour: the bridge builds
  /// an ordinary heap arena that only this process can reach, which is what
  /// §5.8's form 3 — a bridge composed alongside its only consumer — wants.
  ///
  /// Non-empty is what makes a **separate process** able to attach: the bridge
  /// publishes its arena under this name and any consumer joins it read-only
  /// with `tf_tree::open()`, `tft_tree_open()` or `tf_tree.open()`, with no
  /// bridge-specific API on either side. That is §9.1's *"one bridge plus N
  /// `tf_tree` consumers"* arm, and it is unconstructible with this left empty.
  ///
  /// **It is not `time_domain`, and it does not carry a domain at all.** The
  /// *rendezvous* domain comes from `$TF_TREE_DOMAIN`, else `$ROS_DOMAIN_ID`,
  /// else 0 — the convention two robots on one host already use
  /// (`docs/decisions/0019` §3). A consumer therefore needs this name and the
  /// same domain, and nothing else.
  ///
  /// It can fail where an empty one cannot — the name is already held by a live
  /// arena, the runtime directory is unusable, the library was built without
  /// `--features shm` — and every one of those is a `BridgeError` out of the
  /// constructor. **There is no fallback to a heap arena**: a silent downgrade
  /// leaves every consumer waiting on a rendezvous that will never appear.
  std::string arena_name;
};

/// The most recent §5.4 authority conflict, with both publishers named.
///
/// §5.4 calls this "the feature that finds pre-existing bugs in the host
/// system" and the diagnostic goes to the log, where an operator reads it. This
/// struct is the same information in a form a *program* can read — a health
/// topic, a diagnostic aggregator, or a test asserting that §5.3's attribution
/// actually resolved a GID to a node name rather than to
/// `<unknown publisher>`.
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

  /// The QoS the middleware actually gave the `/tf` subscription, read back
  /// from it at construction (§5.2's "log the **negotiated** QoS").
  ///
  /// This exists so §5.2's NORMATIVE table is *assertable*. Both of its fields
  /// regress silently: `best_effort` is compatible with a reliable publisher,
  /// so DDS still matches, nothing errors, and a test that republishes until a
  /// counter moves cannot tell — while on a loaded robot the middleware
  /// discards `/tf` under congestion instead of retransmitting.
  const rclcpp::QoS & actual_tf_qos() const noexcept {return actual_tf_qos_;}

  /// The QoS the middleware actually gave the `/tf_static` subscription. Its
  /// durability is the field §5.2 calls the single most common ROS 2 tf
  /// integration bug.
  const rclcpp::QoS & actual_tf_static_qos() const noexcept {return actual_tf_static_qos_;}

  /// §5.6's remap table, `from` (the name on the wire) to `to` (the name the
  /// arena declares). Complete before the first message, and logged by the
  /// constructor.
  const std::vector<std::pair<std::string, std::string>> & remap() const noexcept
  {
    return remap_;
  }

  /// The most recent §5.4 authority conflict, or `{}` if there has been none.
  /// A snapshot on the same terms as `stats()`, and for the same reason.
  AuthorityConflict last_authority_conflict() const;

private:
  using Gid = std::array<uint8_t, 16>;

  /// One time jump reported by rcl, waiting for the ingest thread to apply it.
  ///
  /// **This exists because the reporting thread is not the bridge's.** rclcpp
  /// fires a jump callback from whichever thread updated the clock — with
  /// `NodeOptions::use_clock_thread` (default `true`) that is the `TimeSource`'s
  /// own `/clock` thread, and on a `use_sim_time` parameter change it is
  /// whichever executor holds the node. Every `tft_bridge_*` entry point is
  /// affinity-checked against the thread that created the bridge: a debug build
  /// of `tf_tree_c` calls `std::process::abort()` and takes the whole ROS
  /// process with it, a release build returns `TFT_ERR_WRONG_THREAD` and the
  /// jump is silently lost. Neither is acceptable, and no amount of testing
  /// makes the direct call safe — so the callback does not call the ABI at all.
  /// It writes here, and `drain_time_jump` applies it on the ingest thread.
  ///
  /// Held by `shared_ptr` and captured by the callback **instead of `this`**.
  /// rclcpp does not synchronize `~JumpHandler` against a callback already
  /// running, so a callback firing while `~BridgeHandle` runs would touch a
  /// destroyed object. Owning the slot separately makes that unrepresentable:
  /// the worst case is a write into a slot nobody will ever drain.
  struct JumpSlot
  {
    std::mutex mutex;
    /// False when there is nothing to apply. The first jump wins; see
    /// `coalesced`.
    bool pending = false;
    /// `rcl_time_jump_t::delta`: **the new time minus the old**, so a rewind is
    /// negative. Passed to the ABI unnegated.
    int64_t delta_nanos = 0;
    tft_bridge_jump_kind kind = 0;
    /// Jumps that arrived while one was already pending. A jump halts or
    /// recreates the bridge, so the *first* one is the transition that matters
    /// and the rest are consequences of it; they are counted rather than
    /// overwriting it, so the log can say the clock was moved more than once.
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

  /// Read back from the subscriptions in the constructor, on the constructing
  /// thread, and never touched again. Initialised to a depth the code below
  /// overwrites; `rclcpp::QoS` has no default constructor.
  rclcpp::QoS actual_tf_qos_{rclcpp::KeepLast(1)};
  rclcpp::QoS actual_tf_static_qos_{rclcpp::KeepLast(1)};

  /// §5.3's per-GID state, touched only from the ingest thread: how many graph
  /// walks this GID has cost, or `kResolved` once one of them matched it.
  std::map<Gid, uint32_t> gid_state_;

  /// **A local monotonic clock, and the only clock this class reads.**
  ///
  /// It has two jobs, and they are the same job. It is the receipt clock the
  /// bridge's step detector measures each publisher's offset against — a
  /// detector whose reference is the clock under suspicion cannot tell a clock
  /// reset from the signal that would reveal one — and it is the clock every
  /// `*_THROTTLE` in this file rate-limits on.
  ///
  /// The clock it is **not** is `node_->get_clock()`. That is `RCL_ROS_TIME`,
  /// which under `use_sim_time` *is* `/clock`: it reads 0 until the first
  /// `/clock` message, so `now >= last_logged + period` is false and every
  /// throttled diagnostic is suppressed over exactly the boot window a
  /// misconfigured bridge is diagnosed in; and it rewinds when a bag loops, so
  /// after a rewind it suppresses diagnostics until sim time has climbed back
  /// past the old mark — the diagnostics *about the rewind*, for the duration
  /// of the rewind.
  ///
  /// `RCL_STEADY_TIME` must be named explicitly: `rclcpp::Clock`'s constructor
  /// defaults to `RCL_SYSTEM_TIME`, so `rclcpp::Clock steady_;` compiles and
  /// silently gives the system clock, which NTP steps.
  ///
  /// Ingest-thread only, like everything above it. `rcutils_steady_time_now` is
  /// documented lock-free and allocation-free, which is what makes one read per
  /// message affordable on this path.
  rclcpp::Clock steady_{RCL_STEADY_TIME};

  /// Whether an arena refusal has been logged yet. Ingest-thread only. The
  /// first one is unconditional; see the `TFT_BRIDGE_REJECTED` arm.
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

  /// Written by the ingest thread before it publishes a failing status through
  /// the constructor's future, and read by the constructor after — which is
  /// what orders the two accesses.
  std::string create_error_;

  /// The hand-off from rcl's jump callback to the ingest thread. Never null.
  std::shared_ptr<JumpSlot> jump_slot_ = std::make_shared<JumpSlot>();

  /// Keeps the registered jump callback alive: rclcpp holds a `weak_ptr`, so
  /// dropping this unregisters and the authoritative path silently never fires
  /// — with nothing failing anywhere. Null when registration was refused, which
  /// is a degradation and not an error (§5.3's rule, applied to a clock instead
  /// of to a publisher name).
  ///
  /// **Declared after `jump_slot_` on purpose.** Members are destroyed in
  /// reverse declaration order, so this one goes first and the callback is
  /// unregistered before the slot it writes into is destroyed. The destructor
  /// resets it explicitly as well, so the ordering does not depend on nobody
  /// ever appending a member below it.
  rclcpp::JumpHandler::SharedPtr jump_handler_;
};

}  // namespace tf_tree_ros

#endif  // TF_TREE_ROS__BRIDGE_HANDLE_HPP_
