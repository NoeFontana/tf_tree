// `docs/PHASE4.md` §5.8 form 3 — the implementation the other two forms wrap.

#include "tf_tree_ros/bridge_handle.hpp"

#include <chrono>
#include <cinttypes>
#include <cstring>
#include <string>
#include <utility>
#include <vector>

#include <rmw/types.h>

namespace tf_tree_ros
{
namespace
{

/// §5.3 matches `rmw_message_info_t::publisher_gid` against the graph's
/// `endpoint_gid()`, and `tft_bridge_offer` takes exactly 16 bytes. Measured on
/// `rmw_fastrtps_cpp` 9.4.8, but the constant is normative across RMWs, so a
/// build against one that disagrees must fail here rather than read past the
/// end of the array.
static_assert(RMW_GID_STORAGE_SIZE >= 16, "a publisher GID must be at least 16 bytes");

/// `builtin_interfaces/Time` to nanoseconds, without `rclcpp::Time`.
///
/// `rclcpp::Time` carries a clock type and asserts on comparisons between
/// mismatched ones. The bridge never compares stamps — `tf_tree_bridge`'s clock
/// guard does, in one domain it was told about at startup (§5.5) — so taking
/// the arithmetic directly avoids importing a second, weaker notion of time
/// domain alongside the engine's typed one.
int64_t stamp_nanos(const builtin_interfaces::msg::Time & t)
{
  return static_cast<int64_t>(t.sec) * 1000000000LL + static_cast<int64_t>(t.nanosec);
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
  // **`automatically_add_to_executor_with_node = false`.** The node's own
  // executor must not pick this group up: every callback on it runs
  // `tft_bridge_offer`, which is only legal on the thread that created the
  // bridge, and that thread is this handle's. Left at the default, a caller who
  // spins the node would get `TFT_ERR_WRONG_THREAD` on every transform.
  group_ = node_->create_callback_group(
    rclcpp::CallbackGroupType::MutuallyExclusive, /*automatically_add_to_executor_with_node=*/
    false);

  rclcpp::SubscriptionOptions sub_opts;
  sub_opts.callback_group = group_;

  // §5.9's amended overflow signal. "Subscription queue depth" is not an API
  // that exists in rclcpp, rcl or rmw; `message_lost_callback` is, it is what
  // the middleware actually reports, and it answers the question §5.9 asks —
  // "is the bridge the bottleneck?" — directly rather than by inference.
  sub_opts.event_callbacks.message_lost_callback =
    [this](rmw_message_lost_status_t & s) {
      RCLCPP_ERROR(
        log_,
        "the middleware dropped %zu TFMessage(s) before the bridge saw them "
        "(%zu total): the ingest thread is not keeping up, or the publisher "
        "outruns KeepLast(%zu)",
        s.total_count_change, s.total_count, opts_.queue_depth);
    };

  // §5.2, NORMATIVE and the single most common ROS 2 tf integration bug: /tf is
  // volatile, /tf_static is transient_local. A volatile subscription to
  // /tf_static receives nothing from a broadcaster that published before this
  // node started, which is most of them, and it presents as "my static
  // transforms are missing" with no error anywhere.
  const auto qos_tf = rclcpp::QoS(rclcpp::KeepLast(opts_.queue_depth)).reliable().durability_volatile();
  const auto qos_static =
    rclcpp::QoS(rclcpp::KeepLast(opts_.queue_depth)).reliable().transient_local();

  // The `std::shared_ptr<const T>` callback signature is what makes §5.8's
  // intra-process claim true: with `use_intra_process_comms`, rclcpp 32 hands
  // this the publisher's own message with no serialization at all — verified
  // here for `transient_local` `/tf_static` and for a late-joining
  // subscription, both of which have historically been exceptions.
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

  std::promise<tft_status> ready;
  auto done = ready.get_future();
  thread_ = std::thread([this, &ready] {run(ready);});
  const tft_status rc = done.get();
  if (rc != TFT_OK) {
    thread_.join();
    // Drop the subscriptions so a caller that catches this is left with the
    // node it handed us, not a node carrying two dead subscriptions on a group
    // no executor drives.
    sub_tf_.reset();
    sub_static_.reset();
    throw BridgeError(rc, create_error_);
  }

  RCLCPP_INFO(
    log_,
    "ingest bridge up: %s KeepLast(%zu) reliable volatile, %s KeepLast(%zu) reliable "
    "transient_local, authority=%d, on_clock_reset=%d, domain=%u",
    opts_.tf_topic.c_str(), opts_.queue_depth, opts_.tf_static_topic.c_str(), opts_.queue_depth,
    static_cast<int>(opts_.authority), static_cast<int>(opts_.on_clock_reset),
    static_cast<unsigned>(opts_.time_domain));

  // §5.6, NORMATIVE: "log the resulting mapping table at startup. A silent
  // remap is worse than no remap." The table is complete here rather than
  // accumulated as frames arrive, because `tft_bridge_create` puts every
  // declared frame through the same normalizer the wire will use.
  for (const auto & row : remap_) {
    RCLCPP_INFO(
      log_, "frame remap: %s on the wire is declared as %s", row.first.c_str(),
      row.second.c_str());
  }
}

BridgeHandle::~BridgeHandle()
{
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

  // Report the configured depth alongside the counters, so a high-water mark
  // reads as a fraction rather than as a bare number.
  tft_bridge_note_queue_depth(bridge_, 0);
  refresh_stats();
  return TFT_OK;
}

void BridgeHandle::run(std::promise<tft_status> & ready)
{
  // Everything the ABI's affinity check cares about happens on this thread:
  // create, every offer, every stats read, and the free at the bottom.
  const tft_status rc = create_bridge();
  ready.set_value(rc);
  if (rc != TFT_OK) {
    return;
  }

  exec_ = std::make_shared<rclcpp::executors::SingleThreadedExecutor>();
  exec_->add_callback_group(group_, node_->get_node_base_interface());

  // **`spin_once(timeout)` rather than `spin()` + `cancel()`, deliberately.**
  // `Executor::cancel()` stores `spinning = false`; `spin()` then does
  // `spinning.exchange(true)` and loops while it is true. A destructor that
  // runs before this thread reaches `spin()` therefore cancels nothing and the
  // join never returns. The poll interval costs only shutdown latency: the
  // wait set still returns the instant a message arrives.
  while (!stop_.load(std::memory_order_relaxed) && rclcpp::ok(node_->get_node_options().context())) {
    exec_->spin_once(std::chrono::milliseconds(50));
  }

  exec_->remove_callback_group(group_);
  exec_.reset();
  tft_bridge_free(bridge_);
  bridge_ = nullptr;
}

void BridgeHandle::ingest(
  const tf2_msgs::msg::TFMessage & msg, const rclcpp::MessageInfo & info, tft_bridge_topic topic)
{
  tft_bridge_note_message(bridge_);

  // §5.3: the GID is 16 bytes of the middleware's own identity, carried on
  // every sample. A GID that resolves to no cached node is not an error — the
  // Rust side degrades it to `<unknown publisher>` and keeps running.
  const uint8_t * gid = info.get_rmw_message_info().publisher_gid.data;

  for (const auto & t : msg.transforms) {
    offer_one(t, gid, topic);
  }

  refresh_stats();
}

void BridgeHandle::offer_one(
  const geometry_msgs::msg::TransformStamped & t, const uint8_t * gid, tft_bridge_topic topic)
{
  tft_bridge_sample s{};
  s.struct_size = static_cast<uint32_t>(sizeof s);
  // **Raw names, on purpose.** §5.6's normalization — the leading `/`, the
  // warn-once, the `tf_prefix` — is the bridge's job, and pre-normalizing here
  // would move that judgment into C++ where it would be a second, divergent
  // implementation.
  s.frame_id = t.header.frame_id.c_str();
  s.child_frame_id = t.child_frame_id.c_str();
  s.stamp_nanos = stamp_nanos(t.header.stamp);
  // `[qw qx qy qz tx ty tz]` — the canonical order (`docs/PHASE1.md` §3.1),
  // **not** `geometry_msgs`' `x y z w`. Getting this backwards produces a valid,
  // different rotation that nothing downstream can detect.
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
    // The status answers a different question from the outcome: it says the
    // *call* was malformed — a name that is not UTF-8, a `struct_size` from
    // another build. Everything that happened to the sample is in `out`.
    RCLCPP_ERROR(
      log_, "tft_bridge_offer rejected the call (%d): %s", rc, last_error_message().c_str());
    return;
  }
  report(out, topic);
}

void BridgeHandle::report(const tft_bridge_outcome & out, tft_bridge_topic topic)
{
  // Nothing here decides anything: `out.action` is the decision, already made.
  // `out.first_time` is the pipeline's own rate limiter, and it is the only
  // reason a 1 kHz misconfigured edge does not emit a thousand lines a second.
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
      if (out.first_time != 0) {
        RCLCPP_WARN(
          log_, "%s -> %s dropped from %s: %s", out.parent, out.child, topic_name(topic),
          out.detail);
      }
      return;

    case TFT_BRIDGE_REJECTED:
      RCLCPP_ERROR(
        log_, "the arena refused %s -> %s (status %d): %s", out.parent, out.child, out.status,
        out.detail);
      return;

    case TFT_BRIDGE_HALT:
      RCLCPP_FATAL(
        log_, "ingest bridge HALTED on %s -> %s: %s. Every later transform is refused.",
        out.parent, out.child, out.detail);
      return;

    case TFT_BRIDGE_RECREATE:
      // §5.5's `recreate` is a *report* here: every `tft_plan` a consumer
      // compiled points into the current arena, so the ABI will not swap it.
      // The owner of this handle destroys it and builds a new one.
      RCLCPP_FATAL(
        log_,
        "the clock went backwards by %" PRId64
        " ns: this bridge is finished and must be replaced. %s",
        out.by_nanos, out.detail);
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

}  // namespace tf_tree_ros
