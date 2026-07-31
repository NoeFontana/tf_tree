// `docs/PHASE4.md` §5 — the ingest path end to end, over a real DDS.
//
// Everything §5 *judges* is already tested in `crates/tf_tree_bridge` and
// `crates/tf_tree_c/tests/bridge.rs`, on every `just test`. What is untestable
// without a middleware, and therefore what is here, is the half this package
// adds: that a `tf2_msgs/TFMessage` published by another participant arrives,
// unpacks into the POD sample in the right order, and lands in the arena where
// a reader can find it.

#include <atomic>
#include <chrono>
#include <memory>
#include <string>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include <rclcpp/rclcpp.hpp>
#include <rcutils/logging.h>
#include <tf2_msgs/msg/tf_message.hpp>

#include "tf_tree_ros/bridge_handle.hpp"

namespace
{

/// One dynamic edge and one static one — the same shape
/// `crates/tf_tree_c/tests/bridge.rs` uses, so a failure here is about the ROS
/// half rather than about the config parser.
constexpr const char * kTopology = R"(
[[edge]]
parent = "odom"
child = "base_link"
kind = "dynamic"
capacity = 256

[[edge]]
parent = "base_link"
child = "lidar"
kind = "static"
pose = [0.9659258262890683, 0.0, 0.0, 0.25881904510252074, 0.35, -0.02, 0.61]
)";

/// A 30 degree yaw with a translation nothing else in the fixture shares, so a
/// read-back that returns identity — or the static edge's pose — fails rather
/// than coincidentally passing.
constexpr double kQw = 0.9659258262890683;
constexpr double kQz = 0.25881904510252074;
constexpr double kTx = 1.5;
constexpr double kTy = -2.25;
constexpr double kTz = 0.75;

constexpr int64_t kStamp = 1'000'000'000LL;

geometry_msgs::msg::TransformStamped make_transform(
  const std::string & parent, const std::string & child, int64_t stamp_ns)
{
  geometry_msgs::msg::TransformStamped t;
  t.header.frame_id = parent;
  t.child_frame_id = child;
  t.header.stamp.sec = static_cast<int32_t>(stamp_ns / 1000000000LL);
  t.header.stamp.nanosec = static_cast<uint32_t>(stamp_ns % 1000000000LL);
  t.transform.rotation.w = kQw;
  t.transform.rotation.x = 0.0;
  t.transform.rotation.y = 0.0;
  t.transform.rotation.z = kQz;
  t.transform.translation.x = kTx;
  t.transform.translation.y = kTy;
  t.transform.translation.z = kTz;
  return t;
}

/// Publish `msg` until the bridge reports at least `want` transforms offered,
/// or the deadline passes.
///
/// Republishing rather than publishing once and sleeping is deliberate: DDS
/// discovery between two participants in one process is not instant, and a
/// single publish into an undiscovered subscription is simply lost. The test
/// would then be a discovery-latency measurement wearing an ingest test's name.
bool pump_until(
  const rclcpp::Publisher<tf2_msgs::msg::TFMessage>::SharedPtr & pub,
  const tf2_msgs::msg::TFMessage & msg, const tf_tree_ros::BridgeHandle & bridge, uint64_t want,
  std::chrono::seconds timeout)
{
  const auto deadline = std::chrono::steady_clock::now() + timeout;
  while (std::chrono::steady_clock::now() < deadline) {
    pub->publish(msg);
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
    if (bridge.stats().transforms >= want) {
      return true;
    }
  }
  return false;
}

class IngestTest : public ::testing::Test
{
protected:
  /// **A topic per test, and never the real `/tf`.** `test_qos`,
  /// `test_attribution` and `test_node` all do this; this fixture used to leave
  /// the defaults, so any other ROS process in the same `ROS_DOMAIN_ID` — a
  /// `ros2 bag play`, a second job on a shared build host — could move these
  /// counters inside the assertion window. The `>=` assertions would usually
  /// survive it; the ledger `EXPECT_EQ` below would not.
  void SetUp() override
  {
    topic_ = std::string("/tf_ingest_") + ::testing::UnitTest::GetInstance()->
      current_test_info()->name();
    node_ = std::make_shared<rclcpp::Node>("tf_tree_ingest_test");
    publisher_node_ = std::make_shared<rclcpp::Node>("tf_broadcaster_under_test");
    pub_ = publisher_node_->create_publisher<tf2_msgs::msg::TFMessage>(
      topic_, rclcpp::QoS(rclcpp::KeepLast(100)).reliable());
  }

  tf_tree_ros::BridgeOptions options() const
  {
    tf_tree_ros::BridgeOptions o;
    o.topology_toml = kTopology;
    o.tf_topic = topic_;
    o.tf_static_topic = topic_ + "_static";
    return o;
  }

  std::string topic_;
  rclcpp::Node::SharedPtr node_;
  rclcpp::Node::SharedPtr publisher_node_;
  rclcpp::Publisher<tf2_msgs::msg::TFMessage>::SharedPtr pub_;
};

/// A `/tf` transform published by a separate participant is written into the
/// arena, and reads back through `tft_plan_at` with the pose that was sent.
///
/// This is the whole of what the ROS half contributes to an applied transform:
/// the QoS that lets it arrive, and the field-by-field unpack into
/// `tft_bridge_sample`.
///
/// **Mutant:** in `BridgeHandle::offer_one`, swap the `s.pose[0]` and
/// `s.pose[3]` assignments — write `rotation.z` into the `qw` slot and
/// `rotation.w` into `qz`. That is `geometry_msgs`' `x y z w` order colliding
/// with `docs/PHASE1.md` §3.1's `qw qx qy qz`, it produces a *valid unit
/// quaternion* so nothing upstream refuses it, and this test then reads back
/// `qw = 0.2588` against the expected `0.9659` and fails. Applied; it dies.
TEST_F(IngestTest, an_applied_transform_reads_back_with_the_pose_that_was_sent)
{
  tf_tree_ros::BridgeHandle bridge(node_.get(), options());

  tf2_msgs::msg::TFMessage msg;
  msg.transforms.push_back(make_transform("odom", "base_link", kStamp));
  ASSERT_TRUE(pump_until(pub_, msg, bridge, 1, std::chrono::seconds(20)));

  const auto stats = bridge.stats();
  EXPECT_GE(stats.applied, 1u);
  EXPECT_EQ(stats.dropped_undeclared, 0u);
  EXPECT_EQ(stats.dropped_bad_pose, 0u);

  tft_plan * plan = nullptr;
  ASSERT_EQ(tft_plan_create(bridge.tree(), "odom", "base_link", &plan), TFT_OK);

  double pose[7] = {0};
  ASSERT_EQ(tft_plan_at(plan, kStamp, TFT_LAYOUT_QVEC7_WXYZ, pose), TFT_OK);
  tft_plan_free(plan);

  EXPECT_NEAR(pose[0], kQw, 1e-12);
  EXPECT_NEAR(pose[1], 0.0, 1e-12);
  EXPECT_NEAR(pose[2], 0.0, 1e-12);
  EXPECT_NEAR(pose[3], kQz, 1e-12);
  EXPECT_NEAR(pose[4], kTx, 1e-12);
  EXPECT_NEAR(pose[5], kTy, 1e-12);
  EXPECT_NEAR(pose[6], kTz, 1e-12);
}

/// A transform for an edge the config does not declare is dropped and counted
/// (§5.8's amendment), and the bridge keeps ingesting the ones it does declare.
///
/// The two halves matter together: an implementation that stopped on the first
/// undeclared edge would pass an assertion on `dropped_undeclared` alone.
///
/// **Mutant:** in `BridgeHandle::ingest`, `break` out of the transform loop
/// instead of continuing after the first sample. `dropped_undeclared` still
/// reaches 1 — the undeclared transform is first in the message — but nothing
/// is ever applied and the `applied >= 1` expectation fails. Applied; it dies.
TEST_F(IngestTest, an_undeclared_edge_is_dropped_without_stopping_the_declared_one)
{
  tf_tree_ros::BridgeHandle bridge(node_.get(), options());

  tf2_msgs::msg::TFMessage msg;
  msg.transforms.push_back(make_transform("map", "odom", kStamp));
  msg.transforms.push_back(make_transform("odom", "base_link", kStamp));
  ASSERT_TRUE(pump_until(pub_, msg, bridge, 2, std::chrono::seconds(20)));

  const auto stats = bridge.stats();
  EXPECT_GE(stats.dropped_undeclared, 1u);
  EXPECT_GE(stats.applied, 1u);
  // §5.9's ledger. It balances on a healthy bridge *and* on this one, which is
  // the only interesting case: a path that returns without counting is exactly
  // how "we are not dropping anything" becomes false with no test failing.
  EXPECT_EQ(
    stats.applied + stats.rejected_by_arena + stats.static_verified + stats.dropped_authority +
    stats.dropped_non_monotonic + stats.dropped_bad_name + stats.dropped_kind_change +
    stats.dropped_undeclared + stats.dropped_bad_pose + stats.refused_after_halt,
    stats.transforms);
}

/// A config the engine will not build fails in the constructor, not at the
/// first message (§5.5 is NORMATIVE about the domain case, and the same
/// argument covers every startup refusal).
///
/// **Mutant:** in `BridgeHandle::run`, call `ready.set_value(TFT_OK)` instead
/// of `ready.set_value(rc)`. The constructor then returns normally with a NULL
/// `bridge_`, this test's `EXPECT_THROW` fails — and every later offer would
/// have dereferenced NULL on the ingest thread. Applied; it dies.
TEST_F(IngestTest, a_config_that_does_not_parse_throws_from_the_constructor)
{
  tf_tree_ros::BridgeOptions o = options();
  o.topology_toml = "[[edge]]\nparent = \"a\"\n";  // no child, no kind
  EXPECT_THROW(tf_tree_ros::BridgeHandle(node_.get(), o), tf_tree_ros::BridgeError);
}

/// **Form 3 refuses an empty topology, exactly as forms 1 and 2 do.**
///
/// An empty config *parses* — it is a legal description of a tree with no edges
/// — and `tft_bridge_create("")` used to return `TFT_OK`. `BridgeNode`'s
/// both-or-neither parameter check was then the only refusal anywhere in the
/// stack, which left this form, the one §5.8 calls "lowest friction … which,
/// for dogfooding, is us", starting clean, logging "ingest bridge up", and
/// reporting 100 % of the robot's traffic as `TFT_BRIDGE_UNDECLARED` behind a
/// `dropped_undeclared` counter nobody watches. Wiring form 3 into an existing
/// node and reading the topology from a typo'd config key is all it takes.
///
/// **Mutant:** delete the `config.edges.is_empty()` refusal in
/// `tft_bridge_create` (`crates/tf_tree_c/src/bridge.rs`). Construction then
/// succeeds and `EXPECT_THROW` reports "it throws nothing". Applied; it dies.
TEST_F(IngestTest, form_3_refuses_a_topology_that_declares_no_edges)
{
  tf_tree_ros::BridgeOptions o = options();
  o.topology_toml = "";
  EXPECT_THROW(tf_tree_ros::BridgeHandle(node_.get(), o), tf_tree_ros::BridgeError);
}

/// **§5.6's remap table crosses the C boundary and is complete at startup**, and
/// a prefixed arena is the one a consumer must look up.
///
/// The Rust half of `tf_prefix` is covered in `crates/tf_tree_c/tests/bridge.rs`.
/// What only this package can cover is the C++ *walk* of the borrowed rows:
/// `tft_bridge_get_remap` returns `const char *` into the handle's own buffers
/// and **overwrites them on the next call**, so the loop has to copy each row
/// before advancing. No test set `tf_prefix`, so `remap_` was always empty, the
/// loop always terminated at `i == 0`, and §5.6's NORMATIVE "log the resulting
/// mapping table at startup" never executed.
///
/// **Mutant:** delete the `tft_bridge_get_remap` loop from
/// `BridgeHandle::create_bridge`. `remap()` is then empty, the startup table is
/// never logged, and the size expectation fails. Applied; it dies.
/// **Mutant:** in the same loop, `break` after the first row. `remap()` then
/// holds one row instead of three, §5.6's table is logged incomplete, and the
/// vector equality fails. Applied; it dies.
TEST_F(IngestTest, a_tf_prefix_is_reported_as_a_remap_table_and_renames_the_arena)
{
  tf_tree_ros::BridgeOptions o = options();
  o.tf_prefix = "robot1";
  tf_tree_ros::BridgeHandle bridge(node_.get(), o);

  // Every declared frame, rewritten. Complete before the first message, because
  // `tft_bridge_create` puts the config's names through the same normalizer the
  // wire will use — a row appearing later could only be a frame the config
  // never declared.
  const std::vector<std::pair<std::string, std::string>> expected{
    {"odom", "robot1/odom"},
    {"base_link", "robot1/base_link"},
    {"lidar", "robot1/lidar"},
  };
  // Exact, and in file order: the pointers `tft_bridge_get_remap` hands back
  // are the *same two buffers* on every call, so a loop that stored them
  // instead of copying would read every row as the last one — an equality on
  // the whole vector is what catches that, where a `size()` check would not.
  EXPECT_EQ(bridge.remap(), expected);

  // And the wire goes through the same normalizer, so an unprefixed publisher
  // still lands — on the prefixed edge, which is the point of setting a prefix.
  tf2_msgs::msg::TFMessage msg;
  msg.transforms.push_back(make_transform("odom", "base_link", kStamp));
  ASSERT_TRUE(pump_until(pub_, msg, bridge, 1, std::chrono::seconds(20)));
  EXPECT_GE(bridge.stats().applied, 1u);
  EXPECT_EQ(bridge.stats().dropped_undeclared, 0u);

  tft_plan * plan = nullptr;
  ASSERT_EQ(
    tft_plan_create(bridge.tree(), "robot1/odom", "robot1/base_link", &plan), TFT_OK);
  tft_plan_free(plan);
}

namespace
{
/// Counts `FATAL` lines while forwarding every record to the handler that was
/// installed before. `rcutils_logging_set_output_handler` is process-global and
/// documented not thread-safe, so it is installed and removed from the test
/// thread while no bridge is running; the counter itself is written from the
/// ingest thread.
std::atomic<int> g_fatal_lines{0};
rcutils_logging_output_handler_t g_previous_handler = nullptr;

void counting_handler(
  const rcutils_log_location_t * location, int severity, const char * name,
  rcutils_time_point_value_t timestamp, const char * format, va_list * args)
{
  if (severity >= RCUTILS_LOG_SEVERITY_FATAL) {
    g_fatal_lines.fetch_add(1);
  }
  // Forwarded exactly once: `args` is a `va_list` and consuming it twice is UB.
  if (g_previous_handler != nullptr) {
    g_previous_handler(location, severity, name, timestamp, format, args);
  }
}
}  // namespace

/// Two dynamic edges with one publisher each — the shape a `/clock` reset
/// actually has, and now the only shape that can produce one.
///
/// It is local to the test below rather than in `kTopology` because
/// `an_undeclared_edge_is_dropped_without_stopping_the_declared_one` publishes
/// `map -> odom` *expecting* it to be undeclared.
constexpr const char * kTwoOwnerTopology = R"(
[[edge]]
parent = "odom"
child = "base_link"
kind = "dynamic"
capacity = 256

[[edge]]
parent = "map"
child = "odom"
kind = "dynamic"
capacity = 256
)";

/// One node publishing one edge, so that two of these are two *publishers* as
/// far as §5.3's attribution and §5.5's step detector are concerned.
class OneEdgeBroadcaster
{
public:
  OneEdgeBroadcaster(
    const std::string & name, const std::string & topic, std::string parent, std::string child)
  : node_(std::make_shared<rclcpp::Node>(name)),
    pub_(node_->create_publisher<tf2_msgs::msg::TFMessage>(
        topic, rclcpp::QoS(rclcpp::KeepLast(100)).reliable())),
    parent_(std::move(parent)), child_(std::move(child))
  {
  }

  void publish_at(int64_t stamp_ns)
  {
    tf2_msgs::msg::TFMessage msg;
    msg.transforms.push_back(make_transform(parent_, child_, stamp_ns));
    pub_->publish(msg);
  }

private:
  rclcpp::Node::SharedPtr node_;
  rclcpp::Publisher<tf2_msgs::msg::TFMessage>::SharedPtr pub_;
  std::string parent_;
  std::string child_;
};

/// **A halted bridge says so once, not once per transform** — and it takes two
/// publishers moving together to halt it at all.
///
/// Two properties, and the second is what makes the first reachable.
///
/// *One witness is not a clock.* This test used to publish a 5 s regression from
/// a single broadcaster on a single edge and expect a stop. That rule was wrong
/// three times over (`docs/decisions/0012`): a node restarting republishes its
/// own history and looks exactly like a rewind, so promoting one source's
/// regression to a halt stops a healthy robot because one publisher bounced.
/// What a `/clock` reset actually looks like is *every* publisher's stamps
/// stepping by the *same* amount at the same moment, and that is what this
/// fixture produces — two nodes, two edges, one shared −5 s step. Their stamps
/// advance with real time before the step so that neither one's offset from the
/// receipt clock is drifting when it happens: a stamp held constant while the
/// steady clock runs is itself a ramp, and a ramp is not a step.
///
/// *And the stop is announced once.* A stop is latched — the ABI answers
/// `TFT_BRIDGE_HALT` to every later transform forever — so an ungated
/// `RCLCPP_FATAL` is one line per transform for the life of the process: on a
/// robot whose `/tf` carries 20 transforms at 100 Hz, 2000 `FATAL` a second,
/// each taking rcutils' logging mutex on the ingest thread and each burying the
/// one line that says what to do. §5.4 requires the diagnostic be "loud,
/// **rate-limited**". The assertion is on the count, not the text: one line is
/// the contract.
///
/// **Mutant:** remove the `if (out.first_time != 0)` guard from `report()`'s
/// `TFT_BRIDGE_HALT` arm. The count goes to one per refused transform and
/// `EXPECT_EQ(fatal, 1)` fails. (Stated, not applied.)
/// **Mutant — APPLIED, in `docker/tf2`, and observed:** delete
/// `s.received_steady_nanos = received_steady_nanos;` from
/// `BridgeHandle::offer_one`. Every sample then carries the ABI's "no receipt
/// clock" sentinel, so neither publisher's offset can be measured, neither
/// regression is a *step*, the two rewinds stay two isolated faults — and
/// nothing halts. `refused_after_halt` never moves and the wait times out. This
/// is the one that pins the whole L0 plumbing; a version of this file that
/// passed without it would be testing the engine and not this package.
///
/// Applied against the real tree and run: `just ros-test` reported
/// `[  FAILED  ] IngestTest.a_clock_reset_is_announced_once_and_not_once_per_refused_transform
/// (20282 ms)`, `83% tests passed, 1 tests failed out of 6`. The 20-second
/// duration is the finding rather than incidental — it is the wait expiring on a
/// halt that can never arrive, because with no receipt clock the offset table has
/// no reference against which a step could exist. Source restored byte-identical
/// afterwards.
/// **Mutant:** publish both rewinds from the *same* `OneEdgeBroadcaster`. Two
/// edges step together but one publisher owns both, which is a restart and not
/// a clock, so the bridge correctly declines to halt and the wait times out —
/// the false quorum that made the second design wrong.
TEST_F(IngestTest, a_clock_reset_is_announced_once_and_not_once_per_refused_transform)
{
  g_fatal_lines.store(0);
  g_previous_handler = rcutils_logging_get_output_handler();
  rcutils_logging_set_output_handler(counting_handler);

  uint64_t refused = 0;
  uint64_t applied = 0;
  uint64_t resets_while_healthy = 0;
  {
    tf_tree_ros::BridgeOptions o = options();
    o.topology_toml = kTwoOwnerTopology;
    tf_tree_ros::BridgeHandle bridge(node_.get(), o);

    OneEdgeBroadcaster wheels("clock_reset_wheel_driver", topic_, "odom", "base_link");
    OneEdgeBroadcaster localizer("clock_reset_localizer", topic_, "map", "odom");

    // **Stamps read from the same steady clock the bridge times receipt with,
    // not incremented by a fixed step per iteration.** The property wanted is a
    // stream whose stamps track wall time, because that is what a real
    // broadcaster produces and it is what leaves each publisher's
    // stamp-minus-receipt offset *flat*. A fixed `+20 ms` per `sleep_for(20ms)`
    // only approximates it: `sleep_for` guarantees a minimum and overshoots, so
    // the stamps fall a little further behind real time every iteration, and a
    // smoothed baseline trails a constant ramp by a fixed multiple of it. On a
    // loaded machine that ramp is a slow step — and a *common-mode* one, since
    // both broadcasters share this clock — which would halt the bridge during
    // the healthy phase and report itself as "the broadcasters never got going".
    // Reading the clock removes the ramp rather than budgeting for it, and
    // `resets_while_healthy` below asserts it is gone rather than assuming.
    const auto period = std::chrono::milliseconds(20);
    const auto t0 = std::chrono::steady_clock::now();
    int64_t rewind = 0;
    const auto stamp_now = [&t0, &rewind] {
        const int64_t elapsed =
          std::chrono::duration_cast<std::chrono::nanoseconds>(
          std::chrono::steady_clock::now() - t0).count();
        return 10 * kStamp + elapsed + rewind;
      };

    // Both publishers, discovered and applying, with enough samples behind them
    // that their offsets are established rather than being learned.
    const auto warm = std::chrono::steady_clock::now() + std::chrono::seconds(20);
    while (std::chrono::steady_clock::now() < warm) {
      const int64_t stamp = stamp_now();
      wheels.publish_at(stamp);
      localizer.publish_at(stamp);
      std::this_thread::sleep_for(period);
      applied = bridge.stats().applied;
      if (applied >= 20) {
        break;
      }
    }
    resets_while_healthy = bridge.stats().clock_resets;

    // The reset: one step, shared, far past §5.5's 100 ms jitter threshold. Both
    // publishers step inside the same message-pair, which is inside any
    // correlation window measured in seconds.
    rewind = -5 * kStamp;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(20);
    while (std::chrono::steady_clock::now() < deadline) {
      const int64_t stamp = stamp_now();
      wheels.publish_at(stamp);
      localizer.publish_at(stamp);
      std::this_thread::sleep_for(period);
      refused = bridge.stats().refused_after_halt;
      if (refused >= 5) {
        break;
      }
    }
  }
  const int fatal = g_fatal_lines.load();
  rcutils_logging_set_output_handler(g_previous_handler);

  ASSERT_GE(applied, 20u)
    << "the two broadcasters never got going, so there was no steady state to step away from";
  // Asserted before the halt is asserted, because a bridge that stopped during
  // the healthy phase would satisfy every expectation below it for the wrong
  // reason — and a false common-mode halt is the one failure this whole design
  // is a response to.
  ASSERT_EQ(resets_while_healthy, 0u)
    << "the bridge decided the clock had moved while both publishers were healthy and their "
       "stamps were tracking wall time";
  ASSERT_GE(refused, 5u)
    << "two publishers stepped by the same -5 s and the bridge did not stop. Either the receipt "
       "clock is not reaching the sample, or §5.3's attribution did not resolve the two GIDs to "
       "two distinct nodes — in which case they are one publisher to the detector and one witness "
       "is never enough";
  EXPECT_EQ(fatal, 1)
    << "the halt was logged " << fatal << " times against " << refused
    << " refused transforms; §5.4 requires the diagnostic be rate-limited";
}

}  // namespace

int main(int argc, char ** argv)
{
  ::testing::InitGoogleTest(&argc, argv);
  rclcpp::init(argc, argv);
  const int rc = RUN_ALL_TESTS();
  rclcpp::shutdown();
  return rc;
}
