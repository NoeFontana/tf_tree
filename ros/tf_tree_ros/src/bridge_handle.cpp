// `docs/PHASE4.md` §5.8 form 3.

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

/// §5.3's GID is 16 bytes; fail the build on an RMW with a smaller one.
static_assert(RMW_GID_STORAGE_SIZE >= 16, "a publisher GID must be at least 16 bytes");

/// `builtin_interfaces/Time` to nanoseconds; the engine owns the clock guard (§5.5).
int64_t stamp_nanos(const builtin_interfaces::msg::Time & t)
{
  return static_cast<int64_t>(t.sec) * 1000000000LL + static_cast<int64_t>(t.nanosec);
}

/// The backward step asked of rcl (§5.5).
constexpr int64_t kClockRewindThresholdNanos = 100000000LL;

/// Consecutive `spin_once` failures tolerated before the ingest thread stops.
constexpr uint32_t kMaxConsecutiveSpinFailures = 100;

/// One `rcl_time_jump_t` as the ABI's kind code; a source change outranks delta.
tft_bridge_jump_kind jump_kind_of(const rcl_time_jump_t & jump)
{
  if (jump.clock_change == RCL_ROS_TIME_ACTIVATED ||
    jump.clock_change == RCL_ROS_TIME_DEACTIVATED)
  {
    return TFT_BRIDGE_JUMP_CLOCK_TYPE_CHANGED;
  }
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

/// §5.5's evidence tier as a sentence (leading space, trailing full stop), or empty.
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

/// The negotiated reliability.
const char * reliability_name(rclcpp::ReliabilityPolicy p)
{
  switch (p) {
    case rclcpp::ReliabilityPolicy::Reliable: return "reliable";
    case rclcpp::ReliabilityPolicy::BestEffort: return "best_effort";
    case rclcpp::ReliabilityPolicy::SystemDefault: return "system_default";
    default: return "unknown";
  }
}

/// The negotiated durability (§5.2).
const char * durability_name(rclcpp::DurabilityPolicy p)
{
  switch (p) {
    case rclcpp::DurabilityPolicy::TransientLocal: return "transient_local";
    case rclcpp::DurabilityPolicy::Volatile: return "volatile";
    case rclcpp::DurabilityPolicy::SystemDefault: return "system_default";
    default: return "unknown";
  }
}

/// The `tft_last_error` message, for startup failures.
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
  // Not on the node's executor: `tft_bridge_offer` is legal only on this thread.
  group_ = node_->create_callback_group(
    rclcpp::CallbackGroupType::MutuallyExclusive, /*automatically_add_to_executor_with_node=*/
    false);

  rclcpp::SubscriptionOptions sub_opts;
  sub_opts.callback_group = group_;

  // §5.9's overflow signal.
  sub_opts.event_callbacks.message_lost_callback =
    [this](rmw_message_lost_status_t & s) {
      RCLCPP_ERROR(
        log_,
        "the middleware dropped %zu TFMessage(s) before the bridge saw them "
        "(%zu total): the ingest thread is not keeping up, or the publisher "
        "outruns KeepLast(%zu)",
        s.total_count_change, s.total_count, opts_.queue_depth);
    };

  // §5.2: warn on any incompatibility event.
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

  // §5.2, NORMATIVE.
  const auto qos_tf = rclcpp::QoS(rclcpp::KeepLast(opts_.queue_depth)).reliable().durability_volatile();
  const auto qos_static =
    rclcpp::QoS(rclcpp::KeepLast(opts_.queue_depth)).reliable().transient_local();

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

  // Read back the negotiated QoS once (§5.2).
  actual_tf_qos_ = sub_tf_->get_actual_qos();
  actual_tf_static_qos_ = sub_static_->get_actual_qos();

  // Before the ingest thread exists, so a startup jump is latched.
  register_jump_callback();

  auto ready = std::make_shared<std::promise<tft_status>>();
  auto done = ready->get_future();
  thread_ = std::thread([this, ready] {run(*ready);});
  const tft_status rc = done.get();
  if (rc != TFT_OK) {
    thread_.join();
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

  for (const auto & row : remap_) {
    RCLCPP_INFO(
      log_, "frame remap: %s on the wire is declared as %s", row.first.c_str(),
      row.second.c_str());
  }
}

BridgeHandle::~BridgeHandle()
{
  // First, so the clock thread stops writing into the slot.
  jump_handler_.reset();
  stop_.store(true, std::memory_order_relaxed);
  if (thread_.joinable()) {
    thread_.join();
  }
  sub_tf_.reset();
  sub_static_.reset();
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
  // `docs/decisions/0015`: empty means a private heap arena.
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

  // `tft_bridge_note_queue_depth` is never called: rclcpp has no such API (§5.9).
  refresh_stats();
  return TFT_OK;
}

void BridgeHandle::register_jump_callback()
{
  // §5.5's authoritative path: ask the time source (`docs/decisions/0012`).
  rcl_jump_threshold_t threshold{};

  threshold.on_clock_change = true;

  // `min_forward` is DISABLED: the first `/clock` message under `use_sim_time`
  // is a forward jump that would stop the bridge; forward steps fall to the
  // engine's common-mode detector.
  threshold.min_forward.nanoseconds = 0;

  threshold.min_backward.nanoseconds = -kClockRewindThresholdNanos;

  auto slot = jump_slot_;

  try {
    // Jump callbacks apply only to the node's `RCL_ROS_TIME` clock.
    jump_handler_ = node_->get_clock()->create_jump_callback(
      [] {},
      [slot](const rcl_time_jump_t & jump) {
        // Must not call the ABI (wrong thread) or throw.
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
    // Diagnostics are never a correctness dependency (§5.3).
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
  // Only `HALT`/`RECREATE` can result; the topic is ignored.
  report(out, TFT_BRIDGE_TOPIC_TF);
  // `/tf` may be silent when the jump arrives; refresh so `stats()` sees it.
  refresh_stats();
}

namespace
{
/// A GID a graph walk has matched.
constexpr uint32_t kResolved = UINT32_MAX;

/// Graph walks one unresolvable GID may cost (§5.3).
constexpr uint32_t kMaxAttempts = 20;
}  // namespace

void BridgeHandle::maybe_attribute(const uint8_t * gid)
{
  Gid key{};
  std::memcpy(key.data(), gid, key.size());

  // Walk before offering: a late cache freezes `<unknown publisher>` as owner (§5.4).
  const auto it = gid_state_.emplace(key, 0u).first;
  if (it->second == kResolved || it->second >= kMaxAttempts) {
    return;
  }
  it->second++;
  if (attribute_from_graph(key)) {
    it->second = kResolved;
  }
}

bool BridgeHandle::attribute_from_graph(const Gid & wanted)
{
  bool found = false;
  // Use the topic the subscription got: remapping does not apply to
  // `get_publishers_info_by_topic`.
  const std::string topics[2] = {sub_tf_->get_topic_name(), sub_static_->get_topic_name()};
  for (const std::string & topic : topics) {
    // An RMW without endpoint introspection throws here (§5.3).
    std::vector<rclcpp::TopicEndpointInfo> endpoints;
    try {
      endpoints = node_->get_publishers_info_by_topic(topic);
    } catch (const std::exception & e) {
      RCLCPP_WARN_THROTTLE(
        log_, steady_, 5000,
        "the graph could not be walked for %s (%s); publishers on it stay unattributed",
        topic.c_str(), e.what());
      continue;
    }
    for (const auto & info : endpoints) {
      const auto & gid = info.endpoint_gid();

      Gid seen{};
      bool all_zero = true;
      for (size_t i = 0; i < seen.size(); i++) {
        seen[i] = gid[i];
        all_zero = all_zero && gid[i] == 0;
      }
      if (all_zero) {
        continue;
      }

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
  const tft_status rc = create_bridge();
  ready.set_value(rc);
  if (rc != TFT_OK) {
    return;
  }

  exec_ = std::make_shared<rclcpp::executors::SingleThreadedExecutor>();
  exec_->add_callback_group(group_, node_->get_node_base_interface());

  // `spin_once(timeout)`, not `spin()` + `cancel()`, which would hang the join if
  // the destructor ran first; failures are counted and backed off.
  uint32_t consecutive_failures = 0;
  while (!stop_.load(std::memory_order_relaxed) && rclcpp::ok(node_->get_node_options().context())) {
    // Nothing may escape a thread entry point; the reachable throw is contained
    // in `attribute_from_graph`.
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
      consecutive_failures = 0;
    } else if (++consecutive_failures >= kMaxConsecutiveSpinFailures) {
      RCLCPP_FATAL(
        log_,
        "the ingest executor failed %u times in a row; the bridge is giving up and will ingest "
        "nothing further. Destroy this handle and build a new one.",
        consecutive_failures);
      stop_.store(true, std::memory_order_relaxed);
    } else {
      const int64_t steps = std::min<uint32_t>(consecutive_failures, 20u);
      std::this_thread::sleep_for(std::chrono::milliseconds(50 * steps));
    }

    // A bag between loops is silent on `/tf` exactly when its clock rewinds.
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
  // One steady read per message, taken first.
  const int64_t received_steady_nanos = steady_.now().nanoseconds();

  // Apply a reported jump before offering.
  drain_time_jump();

  tft_bridge_note_message(bridge_);

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
  // Value-initialised: zero `received_steady_nanos` means "no receipt clock".
  tft_bridge_sample s{};
  s.struct_size = static_cast<uint32_t>(sizeof s);
  // Raw names: §5.6 normalization is the bridge's job, not a second C++ copy.
  s.frame_id = t.header.frame_id.c_str();
  s.child_frame_id = t.child_frame_id.c_str();
  s.stamp_nanos = stamp_nanos(t.header.stamp);
  // Local monotonic receipt reading; its difference from the stamp is the offset.
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
    RCLCPP_ERROR(
      log_, "tft_bridge_offer rejected the call (%d): %s", rc, last_error_message().c_str());
    return;
  }
  report(out, topic);
}

void BridgeHandle::report(const tft_bridge_outcome & out, tft_bridge_topic topic)
{
  // `out.action` decides; this only rate-limits where the ABI sets no `first_time`.
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
        if (out.first_time != 0) {
          RCLCPP_ERROR(
            log_,
            "%s and %s have both been publishing %s -> %s. %s owns it; %s's samples are dropped.",
            out.owner, out.intruder, out.parent, out.child, out.owner, out.intruder);
        }
        // Not `first_time`-gated: the GID may be unresolved at the first conflict.
        const std::lock_guard<std::mutex> guard(stats_mutex_);
        if (!conflict_.observed || conflict_.owner != out.owner ||
          conflict_.intruder != out.intruder || conflict_.parent != out.parent ||
          conflict_.child != out.child)
        {
          conflict_ = AuthorityConflict{true, out.owner, out.intruder, out.parent, out.child};
        }
        return;
      }
      // Other drop reasons throttle one call site each (`docs/decisions/0011` D3).
      if (out.reason == TFT_BRIDGE_REASON_BAD_NAME) {
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
        RCLCPP_WARN_THROTTLE(
          log_, steady_, 5000,
          "%s -> %s went backwards by %" PRId64 " ns on %s: dropped. One publisher going "
          "backwards is that publisher — a restart, or its own jitter — not the clock. "
          "dropped_non_monotonic in `tf_tree doctor` carries the exact count.",
          out.parent, out.child, out.by_nanos, topic_name(topic));
        return;
      }
      // `BAD_POSE`: the only reason left.
      if (out.first_time != 0) {
        RCLCPP_WARN(
          log_, "%s -> %s dropped from %s: %s", out.parent, out.child, topic_name(topic),
          out.detail);
      }
      return;

    case TFT_BRIDGE_REJECTED:
      // The ABI never sets `first_time` here; the first refusal always prints.
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

    // Stops are latched and replayed; gate on `first_time` (§5.4).
    case TFT_BRIDGE_HALT:
      if (out.first_time != 0) {
        char evidence[256];
        describe_evidence(evidence, sizeof evidence, out);
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
      // `recreate` is a report (§5.5): the owner destroys this handle and rebuilds.
      if (out.first_time != 0) {
        char evidence[256];
        describe_evidence(evidence, sizeof evidence, out);
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
