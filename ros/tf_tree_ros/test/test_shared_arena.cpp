// `docs/decisions/0015` steps 3 and 4 — the `arena_name` parameter, and the
// attach it exists to make possible.
//
// `crates/tf_tree_c/tests/bridge_shared.rs` proves the engine half. What is only
// checkable here is the wiring: that a ROS parameter reaches
// `tft_bridge_options::arena_name`. It fails silently (the node runs healthy,
// only a consumer in another process waits forever), so the test is a
// *comparison*: the same node and attach, without the parameter and with it.

#if !defined(TFT_HAVE_SHM)
// `#error`, not `GTEST_SKIP()`: `ros/build.sh` builds `libtf_tree_c.a` with
// `--features bridge,shm`, so a missing `tft_tree_open` is a build regression and
// a skipped test does not gate.
#error "TFT_HAVE_SHM is not defined: libtf_tree_c was built without --features shm, \
or the CMake package's nm probe did not find tft_tree_open in it. See ros/build.sh step 1."
#endif

#include <unistd.h>

#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <memory>
#include <string>
#include <thread>
#include <vector>

#include <gtest/gtest.h>

#include <rclcpp/rclcpp.hpp>

#include "tf_tree_ros/bridge_node.hpp"

namespace
{

using namespace std::chrono_literals;

/// One dynamic edge and one static one, the shape `test_ingest.cpp` and
/// `crates/tf_tree_c/tests/bridge_shared.rs` use. The static edge is what is read
/// back: the builder writes it at creation, so no DDS traffic is needed.
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

/// The static edge's translation, unique in the fixture so an identity read-back fails.
constexpr double kLidarX = 0.35;

constexpr int64_t kStamp = 1'000'000'000LL;

/// A runtime directory nobody else can be using, created once by `main`.
///
/// Isolation is by directory: the rendezvous is selected by `(runtime dir, domain,
/// name)`, and the threat is other processes on the machine (a `tf_tree serve`, a
/// killed earlier run). `mkdtemp`, not the pid, because pids are reused.
const std::string & scratch_dir()
{
  static const std::string dir = [] {
      std::string tmpl = "/tmp/tf_tree_ros_shared-XXXXXX";
      if (::mkdtemp(tmpl.data()) == nullptr) {
        // Before `InitGoogleTest`: no reporter exists; nothing here means anything without isolation.
        std::perror("mkdtemp(/tmp/tf_tree_ros_shared-XXXXXX)");
        std::abort();
      }
      return tmpl;
    }();
  return dir;
}

/// **One arena name for the whole binary**, set into `$TF_TREE_NAME` by `main`.
///
/// Each test destroys its bridge before returning and the run is sequential, so
/// one name suffices; the first test's negative half doubles as the leak
/// assertion. Within `tf_tree_ipc`'s 64-byte limit and a single path component.
std::string arena_name()
{
  return "rosarena-" + std::to_string(::getpid());
}

rclcpp::NodeOptions with(std::vector<rclcpp::Parameter> params)
{
  // Never the real `/tf`.
  params.emplace_back("tf_topic", std::string("/tf_shared_arena_test"));
  params.emplace_back("tf_static_topic", std::string("/tf_shared_arena_test_static"));
  params.emplace_back("topology_config", std::string(kTopology));
  rclcpp::NodeOptions o;
  o.parameter_overrides(params);
  return o;
}

/// `tft_tree_open` until it succeeds or `timeout` passes. The C ABI has no timeout
/// parameter (`Open::await_open` is Rust-only). Returns nullptr on timeout.
tft_tree * open_within(std::chrono::milliseconds timeout)
{
  const auto deadline = std::chrono::steady_clock::now() + timeout;
  for (;;) {
    tft_tree * tree = nullptr;
    if (tft_tree_open(&tree) == TFT_OK) {
      return tree;
    }
    if (std::chrono::steady_clock::now() >= deadline) {
      return nullptr;
    }
    std::this_thread::sleep_for(20ms);
  }
}

/// `tft_tree_open` once, **returning its status**, so the negative half does not
/// spend a timeout. The caller asserts *which* failure it expects; the C ABI
/// collapses every join failure onto `TFT_ERR_INTERNAL` (`docs/decisions/0015`
/// *Failure*), which this still separates from every other way the call can fail.
tft_status open_status_now()
{
  tft_tree * tree = nullptr;
  const tft_status rc = tft_tree_open(&tree);
  if (rc == TFT_OK) {
    tft_tree_free(tree);
  }
  return rc;
}

/// Assert the attached handle is looking at *this* topology, by reading the
/// static edge the builder wrote into it.
void expect_the_topology_is_there(tft_tree * tree)
{
  ASSERT_NE(tree, nullptr);

  // The dynamic edge's frames exist with no sample yet: a plan compiles over the topology.
  tft_plan * dynamic_plan = nullptr;
  EXPECT_EQ(tft_plan_create(tree, "odom", "base_link", &dynamic_plan), TFT_OK)
    << "the attached arena does not know the topology's dynamic edge";
  tft_plan_free(dynamic_plan);

  tft_plan * plan = nullptr;
  ASSERT_EQ(tft_plan_create(tree, "base_link", "lidar", &plan), TFT_OK)
    << "the attached arena does not know the topology's static edge";
  double pose[7] = {0};
  const tft_status rc = tft_plan_at(plan, kStamp, TFT_LAYOUT_QVEC7_WXYZ, pose);
  tft_plan_free(plan);
  ASSERT_EQ(rc, TFT_OK);
  EXPECT_NEAR(pose[4], kLidarX, 1e-12)
    << "the attached arena is not the one this topology built";
}

/// **The crux: the parameter is load-bearing.**
///
/// Two `BridgeNode`s differing in one parameter, against the one name in
/// `$TF_TREE_NAME`, negative first. Both halves must use **one** name, or the
/// negative half asserts only that an unused name is unused. Declared first
/// deliberately (see `arena_name()`): its negative half is the file's leak assertion.
///
/// **Mutant:** in `BridgeNode`'s constructor, overwrite the parameter
/// (`o.arena_name = "";`); every other test in the package still passes.
TEST(SharedArenaTest, the_arena_name_parameter_is_what_a_separate_attach_finds)
{
  const std::string name = arena_name();

  // 1. No `arena_name`: the arena is private and nothing is under the name.
  {
    auto node = std::make_shared<tf_tree_ros::BridgeNode>(with({}));
    ASSERT_EQ(open_status_now(), TFT_ERR_INTERNAL)
      << "a bridge with no arena_name published a rendezvous under " << name
      << ", or joining one failed for a reason other than its absence. §5.8's form 3 exists to "
      << "need no memfd, no lock file and no participant slot.";
  }

  // 2. The same node, plus the parameter.
  {
    auto node = std::make_shared<tf_tree_ros::BridgeNode>(
      with({rclcpp::Parameter("arena_name", name)}));

    tft_tree * tree = open_within(10s);
    ASSERT_NE(tree, nullptr)
      << "no rendezvous appeared under " << name << " within 10 s, so the arena_name parameter "
      << "reached no further than the node. $TF_TREE_RUNTIME_DIR=" << scratch_dir();

    ASSERT_NO_FATAL_FAILURE(expect_the_topology_is_there(tree));
    tft_tree_free(tree);
  }
}

/// §5.8's **form 3** — a caller that owns its own node and fills
/// `BridgeOptions` directly — reaches the same arena.
///
/// **Mutant:** delete the `o.arena_name = ...` line in
/// `BridgeHandle::create_bridge`; this dies, and so does the test above.
TEST(SharedArenaTest, form_3_publishes_the_arena_through_the_options_field)
{
  const std::string name = arena_name();

  auto node = std::make_shared<rclcpp::Node>("tf_tree_shared_arena_form3");
  tf_tree_ros::BridgeOptions o;
  o.topology_toml = kTopology;
  o.tf_topic = "/tf_shared_arena_form3";
  o.tf_static_topic = "/tf_shared_arena_form3_static";
  o.arena_name = name;
  tf_tree_ros::BridgeHandle bridge(node.get(), o);

  tft_tree * tree = open_within(10s);
  ASSERT_NE(tree, nullptr)
    << "no rendezvous appeared under " << name << " within 10 s, so BridgeOptions::arena_name "
    << "reached no further than this struct. $TF_TREE_RUNTIME_DIR=" << scratch_dir();

  ASSERT_NO_FATAL_FAILURE(expect_the_topology_is_there(tree));
  tft_tree_free(tree);
}

/// A second bridge on a name the first already holds refuses to start, and says
/// so through this package's error type rather than by joining an arena it did
/// not size.
///
/// The assertion is on the `tft_status` code, not the exception type
/// (`docs/API.md` §1 R5; `docs/decisions/0015`'s *Failure* section): it must
/// survive `BridgeHandle::run`'s promise and the constructor's throw.
///
/// **Mutant:** publish `TFT_OK` through the promise regardless, or collapse
/// `fn arena_unavailable` in `crates/tf_tree_c/src/bridge.rs` onto
/// `TFT_ERR_INTERNAL`; a type-only assertion survives the second.
TEST(SharedArenaTest, a_second_bridge_on_a_held_name_refuses_to_start)
{
  const std::string name = arena_name();

  auto node = std::make_shared<rclcpp::Node>("tf_tree_shared_arena_held");
  tf_tree_ros::BridgeOptions o;
  o.topology_toml = kTopology;
  o.tf_topic = "/tf_shared_arena_held";
  o.tf_static_topic = "/tf_shared_arena_held_static";
  o.arena_name = name;
  tf_tree_ros::BridgeHandle first(node.get(), o);

  auto second_node = std::make_shared<rclcpp::Node>("tf_tree_shared_arena_held_2");
  try {
    tf_tree_ros::BridgeHandle second(second_node.get(), o);
    FAIL() << "a second bridge on the held name " << name << " started instead of refusing";
  } catch (const tf_tree_ros::BridgeError & e) {
    EXPECT_EQ(e.status(), TFT_ERR_ARENA_UNAVAILABLE)
      << "the refusal crossed the promise, but not as the code an operator can act on: "
      << e.what();
  }

  // The first is still the one serving.
  tft_tree * tree = open_within(10s);
  ASSERT_NE(tree, nullptr) << "the refused second bridge took the first one's arena with it";
  ASSERT_NO_FATAL_FAILURE(expect_the_topology_is_there(tree));
  tft_tree_free(tree);
}

}  // namespace

int main(int argc, char ** argv)
{
  // Before `rclcpp::init` and any test: `setenv` after init races every `getenv`
  // in the rclcpp/RMW threads. `$TF_TREE_DOMAIN` is pinned because it otherwise
  // falls back to `$ROS_DOMAIN_ID`.
  const std::string dir = scratch_dir();
  ::setenv("TF_TREE_RUNTIME_DIR", dir.c_str(), 1);
  ::setenv("TF_TREE_DOMAIN", "0", 1);
  ::setenv("TF_TREE_NAME", arena_name().c_str(), 1);

  ::testing::InitGoogleTest(&argc, argv);
  rclcpp::init(argc, argv);
  const int rc = RUN_ALL_TESTS();
  rclcpp::shutdown();

  std::error_code ignored;
  std::filesystem::remove_all(dir, ignored);
  return rc;
}
