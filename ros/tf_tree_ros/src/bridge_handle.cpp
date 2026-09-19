// `docs/PHASE4.md` §5.8 form 3 — the implementation the other two forms wrap.

#include "tf_tree_ros/bridge_handle.hpp"

#include <algorithm>
#include <chrono>
#include <cinttypes>
#include <cstdio>
#include <cstring>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include <rmw/types.h>

namespace tf_tree_ros
{
namespace
{

/// §5.3's GID is 16 bytes (`tft_bridge_offer`); a build against an RMW with a
/// smaller one must fail here rather than read past the array.
static_assert(RMW_GID_STORAGE_SIZE >= 16, "a publisher GID must be at least 16 bytes");

/// `builtin_interfaces/Time` to nanoseconds without `rclcpp::Time`, which
/// carries a clock type; the engine's clock guard is the one time-domain check
/// (§5.5).
int64_t stamp_nanos(const builtin_interfaces::msg::Time & t)
{
  return static_cast<int64_t>(t.sec) * 1000000000LL + static_cast<int64_t>(t.nanosec);
}

/// The backward step asked of rcl, mirroring the engine's 100 ms reset
/// threshold (§5.5).
constexpr int64_t kClockRewindThresholdNanos = 100000000LL;

/// Consecutive `spin_once` failures tolerated before the ingest thread stops.
constexpr uint32_t kMaxConsecutiveSpinFailures = 100;

/// One `rcl_time_jump_t` as the ABI's kind code; a source change outranks the
/// delta.
tft_bridge_jump_kind jump_kind_of(const rcl_time_jump_t & jump)
{
  if (jump.clock_change == RCL_ROS_TIME_ACTIVATED ||
    jump.clock_change == RCL_ROS_TIME_DEACTIVATED)
  {
    return TFT_BRIDGE_JUMP_CLOCK_TYPE_CHANGED;
  }
  // `delta` is new minus old, so a rewind is negative; passed unnegated.
  return jump.delta.nanoseconds < 0 ? TFT_BRIDGE_JUMP_BACKWARD : TFT_BRIDGE_JUMP_FORWARD;
}

const char * jump_kind_name(tft_bridge_jump_kind k)
{
  switch (k) {
    case TFT_BRIDGE_JUMP_CLOCK_TYPE_CHANGED: return "clock-source-change";
    case TFT_BRIDGE_JUMP_BACKWARD: return "backward";
    case TFT_BRIDGE_JUMP_FORWARD: return "forward";
    default: return "?";
  }
}

/// §5.5's evidence tier ("reported" vs "inferred") as a sentence with a leading
/// space and trailing full stop, or empty; written to a caller buffer so the
/// arms stay allocation-free.
void describe_evidence(char * buf, size_t n, const tft_bridge_outcome & out)
{
  switch (out.clock_evidence) {
    case TFT_BRIDGE_EVIDENCE_REPORTED:
      std::snprintf(
        buf, n, " The ROS clock itself reported a %s jump of %" PRId64 " ns.",
        jump_kind_name(static_cast<tft_bridge_jump_kind>(out.clock_evidence_detail)),
        out.delta_nanos);
      return;
    case TFT_BRIDGE_EVIDENCE_COMMON_MODE:
      std::snprintf(
        buf, n,
        " Inferred: %u publishers stepped by the same %" PRId64 " ns inside one window, which is a "
        "clock and not a restart.",
        out.clock_evidence_detail, out.delta_nanos);
      return;
    default:
      buf[0] = '\0';
      return;
  }
}

const char * action_name(tft_bridge_action a)
{
  switch (a) {
    case TFT_BRIDGE_APPLIED: return "APPLIED";
    case TFT_BRIDGE_STATIC_VERIFIED: return "STATIC_VERIFIED";
    case TFT_BRIDGE_DROPPED: return "DROPPED";
    case TFT_BRIDGE_UNDECLARED: return "UNDECLARED";
    case TFT_BRIDGE_STATIC_CONFLICT: return "STATIC_CONFLICT";
    case TFT_BRIDGE_HALT: return "HALT";
    case TFT_BRIDGE_RECREATE: return "RECREATE";
    case TFT_BRIDGE_REJECTED: return "REJECTED";
    default: return "?";
  }
}

const char * topic_name(tft_bridge_topic t)
{
  return t == TFT_BRIDGE_TOPIC_TF_STATIC ? "/tf_static" : "/tf";
}

/// The negotiated reliability, as §5.2's own vocabulary.
const char * reliability_name(rclcpp::ReliabilityPolicy p)
{
  switch (p) {
    case rclcpp::ReliabilityPolicy::Reliable: return "reliable";
    case rclcpp::ReliabilityPolicy::BestEffort: return "best_effort";
    case rclcpp::ReliabilityPolicy::SystemDefault: return "system_default";
    default: return "unknown";
  }
}

/// The negotiated durability, the field §5.2 is about: a volatile `/tf_static`
/// subscription fails with no error anywhere.
const char * durability_name(rclcpp::DurabilityPolicy p)
{
  switch (p) {
    case rclcpp::DurabilityPolicy::TransientLocal: return "transient_local";
    case rclcpp::DurabilityPolicy::Volatile: return "volatile";
    case rclcpp::DurabilityPolicy::SystemDefault: return "system_default";
    default: return "unknown";
  }
}

/// The message from `tft_last_error`, for the startup failures that throw.
std::string last_error_message()
{
  tft_error e{};
  e.struct_size = static_cast<uint32_t>(sizeof e);
  if (tft_last_error(&e) != TFT_OK) {
    return "(no detail)";
  }
  return std::string(e.message);
}

}  // namespace

BridgeHandle::BridgeHandle(rclcpp::Node * node, BridgeOptions options)
: node_(node), opts_(std::move(options)), log_(node->get_logger().get_child("tf_tree"))
{
  // Not added to the node's executor: every callback runs `tft_bridge_offer`,
  // legal only on this handle's thread (else `TFT_ERR_WRONG_THREAD`).
  group_ = node_->create_callback_group(
    rclcpp::CallbackGroupType::MutuallyExclusive, /*automatically_add_to_executor_with_node=*/
    false);

  rclcpp::SubscriptionOptions sub_opts;
  sub_opts.callback_group = group_;

  // §5.9's overflow signal: rclcpp has no queue-depth API, so use
  // `message_lost_callback`.
  sub_opts.event_callbacks.message_lost_callback =
    [this](rmw_message_lost_status_t & s) {
      RCLCPP_ERROR(
        log_,
        "the middleware dropped %zu TFMessage(s) before the bridge saw them "
        "(%zu total): the ingest thread is not keeping up, or the publisher "
        "outruns KeepLast(%zu)",
        s.total_count_change, s.total_count, opts_.queue_depth);
    };

  // §5.2: warn on any incompatibility event, naming the trap (rclcpp's default
  // handler logs on the node's logger in the middleware's vocabulary).
  sub_opts.event_callbacks.incompatible_qos_callback =
    [this](rclcpp::QOSRequestedIncompatibleQoSInfo & s) {
      RCLCPP_WARN(
        log_,
        "a publisher's QoS is incompatible with this subscription's (policy %d, %d total, "
        "%d new): its samples will never arrive. §5.2 requires reliable /tf and "
        "reliable+transient_local /tf_static; a volatile or best_effort broadcaster "
        "matches neither and reports nothing else anywhere.",
        static_cast<int>(s.last_policy_kind), static_cast<int>(s.total_count),
        static_cast<int>(s.total_count_change));
    };

  // §5.2, NORMATIVE: /tf volatile, /tf_static transient_local (a volatile
  // subscription misses every broadcaster that published before this node).
  const auto qos_tf = rclcpp::QoS(rclcpp::KeepLast(opts_.queue_depth)).reliable().durability_volatile();
  const auto qos_static =
    rclcpp::QoS(rclcpp::KeepLast(opts_.queue_depth)).reliable().transient_local();

  // The `shared_ptr<const T>` signature is what makes §5.8's zero-serialization
  // intra-process path true.
  sub_tf_ = node_->create_subscription<tf2_msgs::msg::TFMessage>(
    opts_.tf_topic, qos_tf,
    [this](std::shared_ptr<const tf2_msgs::msg::TFMessage> m, const rclcpp::MessageInfo & i) {
      ingest(*m, i, TFT_BRIDGE_TOPIC_TF);
    },
    sub_opts);

  sub_static_ = node_->create_subscription<tf2_msgs::msg::TFMessage>(
    opts_.tf_static_topic, qos_static,
    [this](std::shared_ptr<const tf2_msgs::msg::TFMessage> m, const rclcpp::MessageInfo & i) {
      ingest(*m, i, TFT_BRIDGE_TOPIC_TF_STATIC);
    },
    sub_opts);

  // Read back the negotiated QoS once, here (§5.2): the startup line must not
  // restate the request.
  actual_tf_qos_ = sub_tf_->get_actual_qos();
  actual_tf_static_qos_ = sub_static_->get_actual_qos();

  // Before the ingest thread exists, so a startup jump is latched, not missed.
  register_jump_callback();

  // Shared, not a stack local: the ingest thread outlives this constructor.
  auto ready = std::make_shared<std::promise<tft_status>>();
  auto done = ready->get_future();
  thread_ = std::thread([this, ready] {run(*ready);});
  const tft_status rc = done.get();
  if (rc != TFT_OK) {
    thread_.join();
    // Leave the caller's node without dead entities.
    sub_tf_.reset();
    sub_static_.reset();
    throw BridgeError(rc, create_error_);
  }

  RCLCPP_INFO(
    log_,
    "ingest bridge up: %s KeepLast(%zu) %s %s, %s KeepLast(%zu) %s %s, "
    "authority=%d, on_clock_reset=%d, domain=%u",
    opts_.tf_topic.c_str(), actual_tf_qos_.depth(),
    reliability_name(actual_tf_qos_.reliability()), durability_name(actual_tf_qos_.durability()),
    opts_.tf_static_topic.c_str(), actual_tf_static_qos_.depth(),
    reliability_name(actual_tf_static_qos_.reliability()),
    durability_name(actual_tf_static_qos_.durability()),
    static_cast<int>(opts_.authority), static_cast<int>(opts_.on_clock_reset),
    static_cast<unsigned>(opts_.time_domain));

  // §5.6, NORMATIVE: log the remap table at startup; it is complete here.
  for (const auto & row : remap_) {
    RCLCPP_INFO(
      log_, "frame remap: %s on the wire is declared as %s", row.first.c_str(),
      row.second.c_str());
  }
}

BridgeHandle::~BridgeHandle()
{
  // First, so the clock thread stops writing into the slot; belt and braces
  // beside the slot's `shared_ptr` capture, robust to member reordering.
  jump_handler_.reset();
  stop_.store(true, std::memory_order_relaxed);
  if (thread_.joinable()) {
    thread_.join();
  }
  sub_tf_.reset();
  sub_static_.reset();
  // `tft_tree` is `Send + Sync`, so unlike the bridge this may be freed here.
  if (tree_ != nullptr) {
    tft_tree_free(tree_);
    tree_ = nullptr;
  }
}

tft_status BridgeHandle::create_bridge()
{
  tft_bridge_options o{};
  o.struct_size = static_cast<uint32_t>(sizeof o);
  o.authority = static_cast<tft_bridge_authority>(opts_.authority);
  o.on_clock_reset = static_cast<tft_bridge_on_clock_reset>(opts_.on_clock_reset);
  o.domain = opts_.time_domain;
  o.tf_prefix = opts_.tf_prefix.empty() ? nullptr : opts_.tf_prefix.c_str();
  // `docs/decisions/0015`: empty means NULL means a private heap arena.
  // `struct_size` covers this field (header drift-checked by `just c-header-check`).
  o.arena_name = opts_.arena_name.empty() ? nullptr : opts_.arena_name.c_str();

  tft_status rc = tft_bridge_create(opts_.topology_toml.c_str(), &o, &bridge_);
  if (rc != TFT_OK) {
    create_error_ = "tft_bridge_create: " + last_error_message();
    return rc;
  }

  rc = tft_bridge_tree(bridge_, &tree_);
  if (rc != TFT_OK) {
    create_error_ = "tft_bridge_tree: " + last_error_message();
    tft_bridge_free(bridge_);
    bridge_ = nullptr;
    return rc;
  }

  tft_bridge_remap r{};
  r.struct_size = static_cast<uint32_t>(sizeof r);
  for (uint32_t i = 0; tft_bridge_get_remap(bridge_, i, &r) == TFT_OK; i++) {
    remap_.emplace_back(r.from, r.to);
  }

  // `tft_bridge_note_queue_depth` is never called (`queue_high_water` stays 0):
  // no such API exists in rclcpp (§5.9's amendment); loss is reported through
  // `message_lost_callback`.
  refresh_stats();
  return TFT_OK;
}

void BridgeHandle::register_jump_callback()
{
  // §5.5's authoritative path: ask the time source, not the publishers under
  // suspicion (`docs/decisions/0012`).
  rcl_jump_threshold_t threshold{};

  threshold.on_clock_change = true;

  // `min_forward` is DISABLED on purpose: rcl fires callbacks for every `/clock`
  // message, and the first one under `use_sim_time` is a forward jump of
  // seconds or decades, so any finite threshold stops the bridge at startup. A
  // forward step is left to the engine's common-mode detector ("reported"
  // degrades to "inferred", never to "unnoticed").
  threshold.min_forward.nanoseconds = 0;

  // `/clock` does not go backwards normally, so a backward step is the fault.
  threshold.min_backward.nanoseconds = -kClockRewindThresholdNanos;

  // Captured by value; never `this` (see `JumpSlot`).
  auto slot = jump_slot_;

  try {
    // Registered on the node's `RCL_ROS_TIME` clock (jump callbacks apply only to
    // it); the node outlives this handle by contract.
    jump_handler_ = node_->get_clock()->create_jump_callback(
      // The pre-callback receives no `rcl_time_jump_t`, so act on the post-callback.
      [] {},
      [slot](const rcl_time_jump_t & jump) {
        // May not call the ABI (wrong thread) and must not throw (escaping is
        // `std::terminate`): record for `drain_time_jump`.
        try {
          const std::lock_guard<std::mutex> guard(slot->mutex);
          if (slot->pending) {
            slot->coalesced++;
            return;
          }
          slot->pending = true;
          slot->delta_nanos = jump.delta.nanoseconds;
          slot->kind = jump_kind_of(jump);
        } catch (...) {
        }
      },
      threshold);
  } catch (const std::exception & e) {
    // A diagnostic is never a correctness dependency (§5.3): without the
    // callback the bridge falls back to inferring a reset from publisher stamps.
    RCLCPP_WARN(
      log_,
      "no time-jump callback could be registered (%s): a /clock reset will be inferred from "
      "publisher stamps rather than reported by the time source. That is the designed fallback, "
      "not a failure.",
      e.what());
  }
}

void BridgeHandle::drain_time_jump()
{
  int64_t delta = 0;
  tft_bridge_jump_kind kind = 0;
  uint64_t coalesced = 0;
  {
    const std::lock_guard<std::mutex> guard(jump_slot_->mutex);
    if (!jump_slot_->pending) {
      return;
    }
    delta = jump_slot_->delta_nanos;
    kind = jump_slot_->kind;
    coalesced = jump_slot_->coalesced;
    jump_slot_->pending = false;
    jump_slot_->coalesced = 0;
  }

  if (coalesced != 0) {
    RCLCPP_WARN(
      log_,
      "the ROS clock moved %" PRIu64 " more time(s) before the first jump could be applied; the "
      "first one is the transition being reported",
      coalesced);
  }

  tft_bridge_outcome out{};
  out.struct_size = static_cast<uint32_t>(sizeof out);
  const tft_status rc = tft_bridge_note_time_jump(bridge_, delta, kind, &out);
  if (rc != TFT_OK) {
    RCLCPP_ERROR(
      log_, "tft_bridge_note_time_jump rejected the call (%d): %s", rc,
      last_error_message().c_str());
    return;
  }
  // Only `HALT`/`RECREATE` can result and `report` ignores the topic; sharing it
  // renders and rate-limits a clock stop like a transform stop.
  report(out, TFT_BRIDGE_TOPIC_TF);
  // `/tf` may be silent when the jump arrives (a bag between takes); refresh so
  // the counter reaches `stats()`.
  refresh_stats();
}

namespace
{
/// A GID that a graph walk has matched. Distinct from any attempt count.
constexpr uint32_t kResolved = UINT32_MAX;

/// Graph walks one unresolvable GID may cost (§5.3: keep running on an RMW with
/// no usable GIDs).
constexpr uint32_t kMaxAttempts = 20;
}  // namespace

void BridgeHandle::maybe_attribute(const uint8_t * gid)
{
  Gid key{};
  std::memcpy(key.data(), gid, key.size());

  // Walk before offering: under `FirstWriterWins` the first sample names the
  // edge's owner, and a late cache freezes `<unknown publisher>` in as owner
  // (§5.4). On a new GID, not a graph event, to stay on the ingest thread.
  const auto it = gid_state_.emplace(key, 0u).first;
  if (it->second == kResolved || it->second >= kMaxAttempts) {
    return;
  }
  it->second++;
  if (attribute_from_graph(key)) {
    it->second = kResolved;
  }
  // Else the endpoint is not in the graph yet; the next message walks again.
}

bool BridgeHandle::attribute_from_graph(const Gid & wanted)
{
  bool found = false;
  // The topic the subscription actually got: ROS remapping applies to
  // `create_subscription` but not to `get_publishers_info_by_topic`. An
  // unresolved GID makes two broadcasters compare equal, silently disabling
  // §5.4's detection (`docs/PHASE4.md` §0.0).
  const std::string topics[2] = {sub_tf_->get_topic_name(), sub_static_->get_topic_name()};
  for (const std::string & topic : topics) {
    // §5.3: an RMW without endpoint introspection throws here. Contained at this
    // site, not only at `run`, since `maybe_attribute` runs before the transform
    // loop and an outer catch would lose all ingest.
    std::vector<rclcpp::TopicEndpointInfo> endpoints;
    try {
      endpoints = node_->get_publishers_info_by_topic(topic);
    } catch (const std::exception & e) {
      // `steady_`, not the node clock (see `steady_` in the header).
      RCLCPP_WARN_THROTTLE(
        log_, steady_, 5000,
        "the graph could not be walked for %s (%s); publishers on it stay unattributed",
        topic.c_str(), e.what());
      continue;
    }
    for (const auto & info : endpoints) {
      const auto & gid = info.endpoint_gid();

      // An all-zero GID is "nothing to report"; `tft_bridge_attribute` refuses it.
      Gid seen{};
      bool all_zero = true;
      for (size_t i = 0; i < seen.size(); i++) {
        seen[i] = gid[i];
        all_zero = all_zero && gid[i] == 0;
      }
      if (all_zero) {
        continue;
      }

      // `node_namespace()` is "/" for the default namespace.
      std::string name = info.node_namespace();
      if (name.empty() || name.back() != '/') {
        name += '/';
      }
      name += info.node_name();

      tft_bridge_attribute(bridge_, seen.data(), name.c_str());
      found = found || seen == wanted;
    }
  }
  return found;
}

void BridgeHandle::run(std::promise<tft_status> & ready)
{
  // Create, offer, stats and free all happen on this thread (ABI affinity).
  const tft_status rc = create_bridge();
  ready.set_value(rc);
  if (rc != TFT_OK) {
    return;
  }

  exec_ = std::make_shared<rclcpp::executors::SingleThreadedExecutor>();
  exec_->add_callback_group(group_, node_->get_node_base_interface());

  // `spin_once(timeout)` rather than `spin()` + `cancel()`: a destructor running
  // before `spin()` starts would cancel nothing and hang the join. A failing
  // `spin_once` returns at once, so failures are counted, backed off, and
  // eventually fatal rather than a silent 100 % busy loop.
  uint32_t consecutive_failures = 0;
  while (!stop_.load(std::memory_order_relaxed) && rclcpp::ok(node_->get_node_options().context())) {
    // Nothing may leave a `std::thread` entry point (`std::terminate` kills the
    // host process; `docs/PHASE4.md` §3.4). This is the backstop; the reachable
    // throw is contained in `attribute_from_graph`.
    bool failed = false;
    try {
      exec_->spin_once(std::chrono::milliseconds(50));
    } catch (const std::exception & e) {
      failed = true;
      RCLCPP_ERROR_THROTTLE(
        log_, steady_, 5000,
        "contained an exception from the ingest callback: %s; the bridge keeps ingesting", e.what());
    } catch (...) {
      failed = true;
      RCLCPP_ERROR_THROTTLE(
        log_, steady_, 5000,
        "contained a non-std exception from the ingest callback; the bridge keeps ingesting");
    }

    if (!failed) {
      // Consecutive, so an occasional throw never accumulates to fatal.
      consecutive_failures = 0;
    } else if (++consecutive_failures >= kMaxConsecutiveSpinFailures) {
      // Once per handle life; explains why every counter stopped.
      RCLCPP_FATAL(
        log_,
        "the ingest executor failed %u times in a row; the bridge is giving up and will ingest "
        "nothing further. Destroy this handle and build a new one.",
        consecutive_failures);
      stop_.store(true, std::memory_order_relaxed);
    } else {
      // Linear backoff to a 1 s ceiling.
      const int64_t steps = std::min<uint32_t>(consecutive_failures, 20u);
      std::this_thread::sleep_for(std::chrono::milliseconds(50 * steps));
    }

    // Also drained here: a bag between loops is silent on `/tf` exactly when its
    // clock rewinds.
    drain_time_jump();
  }

  exec_->remove_callback_group(group_);
  exec_.reset();
  tft_bridge_free(bridge_);
  bridge_ = nullptr;
}

void BridgeHandle::ingest(
  const tf2_msgs::msg::TFMessage & msg, const rclcpp::MessageInfo & info, tft_bridge_topic topic)
{
  // One steady read per message (not per transform), taken first: the step
  // detector measures one offset per message per publisher, against a clock
  // independent of the one under suspicion.
  const int64_t received_steady_nanos = steady_.now().nanoseconds();

  // Apply a reported jump before offering, or a message of new-time-base
  // transforms lands in an arena already known to be finished.
  drain_time_jump();

  tft_bridge_note_message(bridge_);

  // §5.3: an unresolved GID degrades to `<unknown publisher>` on the Rust side.
  const uint8_t * gid = info.get_rmw_message_info().publisher_gid.data;
  maybe_attribute(gid);

  for (const auto & t : msg.transforms) {
    offer_one(t, gid, topic, received_steady_nanos);
  }

  refresh_stats();
}

void BridgeHandle::offer_one(
  const geometry_msgs::msg::TransformStamped & t, const uint8_t * gid, tft_bridge_topic topic,
  int64_t received_steady_nanos)
{
  // Value-initialised: a later ABI field is zero, and zero `received_steady_nanos`
  // means "no receipt clock" (inference path).
  tft_bridge_sample s{};
  s.struct_size = static_cast<uint32_t>(sizeof s);
  // Raw names: §5.6 normalization is the bridge's job, not a second C++ copy.
  s.frame_id = t.header.frame_id.c_str();
  s.child_frame_id = t.child_frame_id.c_str();
  s.stamp_nanos = stamp_nanos(t.header.stamp);
  // `stamp_nanos` is the broadcaster's claim; `received_steady_nanos` is a local
  // monotonic reading no publisher can move, and their difference is the offset
  // the engine subtracts. A parameter so "one read per message" is enforced by
  // the signature.
  s.received_steady_nanos = received_steady_nanos;
  // `[qw qx qy qz tx ty tz]` (`docs/PHASE1.md` §3.1), not `geometry_msgs`' xyzw.
  s.pose[0] = t.transform.rotation.w;
  s.pose[1] = t.transform.rotation.x;
  s.pose[2] = t.transform.rotation.y;
  s.pose[3] = t.transform.rotation.z;
  s.pose[4] = t.transform.translation.x;
  s.pose[5] = t.transform.translation.y;
  s.pose[6] = t.transform.translation.z;

  tft_bridge_outcome out{};
  out.struct_size = static_cast<uint32_t>(sizeof out);
  const tft_status rc = tft_bridge_offer(bridge_, topic, &s, gid, &out);
  if (rc != TFT_OK) {
    // The status says the call was malformed; what happened to the sample is in `out`.
    RCLCPP_ERROR(
      log_, "tft_bridge_offer rejected the call (%d): %s", rc, last_error_message().c_str());
    return;
  }
  report(out, topic);
}

void BridgeHandle::report(const tft_bridge_outcome & out, tft_bridge_topic topic)
{
  // `out.action` is the decision; this only rate-limits. Where the ABI sets
  // `first_time` (`UNDECLARED`, `STATIC_CONFLICT`, `NOT_THE_OWNER`, `HALT`,
  // `RECREATE`) use it; `REJECTED` and the other drop reasons never set it, so
  // they throttle here instead.
  switch (out.action) {
    case TFT_BRIDGE_APPLIED:
    case TFT_BRIDGE_STATIC_VERIFIED:
      return;

    case TFT_BRIDGE_UNDECLARED:
      if (out.first_time != 0) {
        RCLCPP_WARN(
          log_,
          "%s carries %s -> %s, which the topology config does not declare: dropped. "
          "Add it to the config, or regenerate one with `tf_tree topology --discover`.",
          topic_name(topic), out.parent, out.child);
      }
      return;

    case TFT_BRIDGE_STATIC_CONFLICT:
      if (out.first_time != 0) {
        RCLCPP_ERROR(
          log_,
          "static conflict on %s -> %s: %s published [%g %g %g %g %g %g %g] and %s published "
          "[%g %g %g %g %g %g %g]. Two robot_state_publishers with different URDFs is the usual "
          "cause.",
          out.parent, out.child, out.owner, out.existing[0], out.existing[1], out.existing[2],
          out.existing[3], out.existing[4], out.existing[5], out.existing[6], out.intruder,
          out.offered[0], out.offered[1], out.offered[2], out.offered[3], out.offered[4],
          out.offered[5], out.offered[6]);
      }
      return;

    case TFT_BRIDGE_DROPPED:
      if (out.reason == TFT_BRIDGE_REASON_NOT_THE_OWNER) {
        // §5.4: name both nodes and the edge; the reason §5.3's attribution exists.
        if (out.first_time != 0) {
          RCLCPP_ERROR(
            log_,
            "%s and %s have both been publishing %s -> %s. %s owns it; %s's samples are dropped.",
            out.owner, out.intruder, out.parent, out.child, out.owner, out.intruder);
        }
        // The record is not `first_time`-gated: a GID can still be unresolved at
        // the edge's first conflict, which would freeze `<unknown publisher>` in.
        // Rewrite whenever the four strings change.
        const std::lock_guard<std::mutex> guard(stats_mutex_);
        if (!conflict_.observed || conflict_.owner != out.owner ||
          conflict_.intruder != out.intruder || conflict_.parent != out.parent ||
          conflict_.child != out.child)
        {
          conflict_ = AuthorityConflict{true, out.owner, out.intruder, out.parent, out.child};
        }
        return;
      }
      // The other drop reasons throttle, one call site each (`docs/decisions/0011`
      // D3): the ABI sets no `first_time` on them, and one flag would be wrong
      // (`NON_MONOTONIC` is high frequency; `BAD_NAME` keys are publisher-chosen
      // and unbounded). rcutils keeps a throttle per macro expansion, so separate
      // sites stop a 1 kHz reason starving the rest. Exact counts are
      // `tft_bridge_stats::dropped_*` (`tf_tree doctor`).
      if (out.reason == TFT_BRIDGE_REASON_BAD_NAME) {
        // Raw wire names: normalization is what failed.
        RCLCPP_WARN_THROTTLE(
          log_, steady_, 5000,
          "%s carries a frame name that does not normalize (%s -> %s): dropped. "
          "dropped_bad_name in `tf_tree doctor` carries the exact count.",
          topic_name(topic), out.parent, out.child);
        return;
      }
      if (out.reason == TFT_BRIDGE_REASON_KIND_CHANGE) {
        RCLCPP_WARN_THROTTLE(
          log_, steady_, 5000,
          "%s -> %s arrived on %s, but that edge is already established as the other kind: "
          "dropped. An edge is static or dynamic, never both.",
          out.parent, out.child, topic_name(topic));
        return;
      }
      if (out.reason == TFT_BRIDGE_REASON_NON_MONOTONIC) {
        // One regressing source is dropped at any magnitude and never stops the
        // bridge; a real reset arrives as `HALT` or `RECREATE`.
        RCLCPP_WARN_THROTTLE(
          log_, steady_, 5000,
          "%s -> %s went backwards by %" PRId64 " ns on %s: dropped. One publisher going "
          "backwards is that publisher — a restart, or its own jitter — not the clock. "
          "dropped_non_monotonic in `tf_tree doctor` carries the exact count.",
          out.parent, out.child, out.by_nanos, topic_name(topic));
        return;
      }
      // `BAD_POSE` is the only reason left, still `first_time`-gated (which the
      // ABI never sets on a drop, so this is dead; not D3's to change).
      if (out.first_time != 0) {
        RCLCPP_WARN(
          log_, "%s -> %s dropped from %s: %s", out.parent, out.child, topic_name(topic),
          out.detail);
      }
      return;

    case TFT_BRIDGE_REJECTED:
      // The first refusal always prints and the rest throttle: the ABI never sets
      // `first_time` here, and a throttle alone is silent through a `use_sim_time`
      // boot on a clock that reads below one period. An arena refusal of a
      // declared transform is the line the operator needs.
      if (!rejected_reported_) {
        rejected_reported_ = true;
        RCLCPP_ERROR(
          log_, "the arena refused %s -> %s (status %d): %s", out.parent, out.child, out.status,
          out.detail);
        return;
      }
      RCLCPP_ERROR_THROTTLE(
        log_, steady_, 5000, "the arena refused %s -> %s (status %d): %s", out.parent,
        out.child, out.status, out.detail);
      return;

    // A stop is latched and replayed for every later transform, so both stops
    // are gated on `first_time` (1 on the stopping offer, 0 on replays); §5.4
    // wants the diagnostic "loud, rate-limited".
    case TFT_BRIDGE_HALT:
      if (out.first_time != 0) {
        char evidence[256];
        describe_evidence(evidence, sizeof evidence, out);
        // `parent`/`child` are empty when no sample was in hand (a `Strict` window
        // closing, a reported jump); the sentence has two shapes.
        if (out.parent[0] == '\0' && out.child[0] == '\0') {
          RCLCPP_FATAL(
            log_, "ingest bridge HALTED: %s.%s Every later transform is refused.",
            out.detail, evidence);
        } else {
          RCLCPP_FATAL(
            log_, "ingest bridge HALTED on %s -> %s: %s.%s Every later transform is refused.",
            out.parent, out.child, out.detail, evidence);
        }
      }
      return;

    case TFT_BRIDGE_RECREATE:
      // §5.5's `recreate` is a report: compiled plans point into the current
      // arena, so the owner destroys this handle and builds a new one.
      if (out.first_time != 0) {
        char evidence[256];
        describe_evidence(evidence, sizeof evidence, out);
        // `delta_nanos` is new minus old (negative for a rewind); forward resets
        // reach this arm too.
        RCLCPP_FATAL(
          log_,
          "the clock jumped by %" PRId64
          " ns: this bridge is finished and must be replaced. %s%s",
          out.delta_nanos, out.detail, evidence);
      }
      return;

    default:
      RCLCPP_ERROR(log_, "unknown bridge action %s (%d)", action_name(out.action), out.action);
      return;
  }
}

void BridgeHandle::refresh_stats()
{
  tft_bridge_stats s{};
  s.struct_size = static_cast<uint32_t>(sizeof s);
  if (tft_bridge_get_stats(bridge_, &s) != TFT_OK) {
    return;
  }
  const std::lock_guard<std::mutex> guard(stats_mutex_);
  stats_ = s;
}

tft_bridge_stats BridgeHandle::stats() const
{
  const std::lock_guard<std::mutex> guard(stats_mutex_);
  return stats_;
}

AuthorityConflict BridgeHandle::last_authority_conflict() const
{
  const std::lock_guard<std::mutex> guard(stats_mutex_);
  return conflict_;
}

}  // namespace tf_tree_ros
