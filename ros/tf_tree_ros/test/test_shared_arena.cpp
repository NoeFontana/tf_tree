// `docs/decisions/0015` steps 3 and 4 — the `arena_name` parameter, and the
// attach it exists to make possible.
//
// `crates/tf_tree_c/tests/bridge_shared.rs` already proves the *engine* half:
// that `tft_bridge_create` with an `arena_name` publishes a rendezvous a second
// **process** can find, and that a NULL one publishes nothing. None of that
// says anything about this package. What is only checkable here is the wiring
// between them — that a ROS parameter reaches
// `tft_bridge_options::arena_name` at all — and it is wiring that fails
// silently: a `BridgeNode` that read the parameter and dropped it on the floor
// starts cleanly, ingests normally, reports healthy counters, and passes every
// other test in this package. The only symptom is a consumer in another process
// that waits forever, which is precisely the failure `0015` refuses to allow.
//
// So the test is a *comparison*, not a happy path: the same node, the same
// topology and the same attach, run once without the parameter and once with
// it. A test that only asserted the second half would pass just as well against
// a `create_bridge()` that never set the field.

#if !defined(TFT_HAVE_SHM)
// **`#error`, not `GTEST_SKIP()`.** `ros/build.sh` builds `libtf_tree_c.a` with
// `--features bridge,shm` and refuses to go on if `tft_tree_open` is missing
// from it, and the CMake package defines `TFT_HAVE_SHM` by probing that same
// archive. Arriving here means one of those two broke, which is a build
// regression rather than an environment this test cannot run in — and a skipped
// test is a test that does not gate.
#error "TFT_HAVE_SHM is not defined: libtf_tree_c was built without --features shm, \
or the CMake package's nm probe did not find tft_tree_open in it. See ros/build.sh step 1."
#endif

#include <unistd.h>

#include <chrono>
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

/// One dynamic edge and one static one, the same shape `test_ingest.cpp` and
/// `crates/tf_tree_c/tests/bridge_shared.rs` use.
///
/// **The static edge is what this test reads back**, and that is deliberate: it
/// is written by the *builder*, when the arena is created, so the attach can
/// assert it saw the bridge's topology without any DDS traffic having to
/// arrive first. What is being tested here is the arena's reachability, not the
/// ingest path; `test_ingest.cpp` owns that and would be the thing failing if
/// the unpack were wrong.
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

/// The static edge's translation, which nothing else in the fixture shares — so
/// a read-back that returned identity fails rather than coincidentally passing.
constexpr double kLidarX = 0.35;

constexpr int64_t kStamp = 1'000'000'000LL;

/// A runtime directory nobody else can be using.
///
/// **Isolation here is by directory, not by name.** The rendezvous is selected
/// by `(runtime dir, domain, name)`, and ctests in this package run in
/// parallel — as does whatever a developer has running on the same machine. A
/// unique `$TF_TREE_RUNTIME_DIR` makes every one of those unreachable from this
/// process and this process unreachable from them, whatever names collide. The
/// arena names below are unique as well, but that is belt and braces: it is the
/// directory that makes a live robot arena on this host impossible to touch.
std::string scratch_dir()
{
  return "/tmp/tf_tree_ros_shared-" + std::to_string(::getpid());
}

/// `<suffix>` under this process, inside `tf_tree_ipc`'s 64-byte limit and a
/// single path component.
std::string arena_name(const char * suffix)
{
  return "rosarena-" + std::to_string(::getpid()) + "-" + suffix;
}

rclcpp::NodeOptions with(std::vector<rclcpp::Parameter> params)
{
  // Never the real `/tf`: every other suite here does the same, and this one
  // subscribes for the whole of its runtime while asserting things about an
  // arena's contents.
  params.emplace_back("tf_topic", std::string("/tf_shared_arena_test"));
  params.emplace_back("tf_static_topic", std::string("/tf_shared_arena_test_static"));
  params.emplace_back("topology_config", std::string(kTopology));
  rclcpp::NodeOptions o;
  o.parameter_overrides(params);
  return o;
}

/// `tft_tree_open` until it succeeds or `timeout` passes.
///
/// The bounded poll is not decoration. `BridgeHandle`'s constructor blocks until
/// the ingest thread has created the bridge, so by the time a `BridgeNode`
/// exists the rendezvous is already served — but there is **no timeout
/// parameter on the C ABI**: `Open::await_open` is Rust-only and deliberately
/// not exposed. A busy machine that made this racy would otherwise turn into a
/// flake with `TFT_ERR_INTERNAL` and no statement of what was being waited for.
///
/// Returns nullptr on timeout; the caller says what never appeared.
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

/// `tft_tree_open` once, expecting failure — the negative half, which must not
/// spend a timeout proving a rendezvous that was never published is absent.
bool opens_now()
{
  tft_tree * tree = nullptr;
  if (tft_tree_open(&tree) != TFT_OK) {
    return false;
  }
  tft_tree_free(tree);
  return true;
}

/// Assert the attached handle is looking at *this* topology, by reading the
/// static edge the builder wrote into it.
void expect_the_topology_is_there(tft_tree * tree)
{
  ASSERT_NE(tree, nullptr);

  // The dynamic edge's frames exist even with no sample in them yet: a plan
  // compiles over the topology, which is what the bridge declared.
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
/// Two `BridgeNode`s differing in one parameter, with the same name in
/// `$TF_TREE_NAME` for both, and the negative first so it cannot be polluted by
/// the positive. Without `arena_name` nothing is reachable under that name;
/// with it, the attach succeeds and sees the topology.
///
/// Running both halves against **one** name is the whole design. A negative
/// half that used a different name would assert only that an unused name is
/// unused, which is true of any implementation whatsoever.
///
/// **Mutant:** in `BridgeNode`'s constructor, ignore the parameter — read it
/// and then overwrite it, `o.arena_name = "";`. That is the node-layer half of
/// the wiring, the half `form_3_publishes_the_arena_through_the_options_field`
/// below cannot see, and it is the failure this test exists for: the node
/// constructs, the bridge runs, and every other test in this package passes.
/// Applied; it dies.
TEST(SharedArenaTest, the_arena_name_parameter_is_what_a_separate_attach_finds)
{
  const std::string name = arena_name("param");
  ASSERT_EQ(::setenv("TF_TREE_NAME", name.c_str(), 1), 0);

  // 1. Today's default: no `arena_name` at all. The arena is private to the
  //    bridge's own process and there is nothing under the name to find.
  {
    auto node = std::make_shared<tf_tree_ros::BridgeNode>(with({}));
    ASSERT_FALSE(opens_now())
      << "a bridge with no arena_name published a rendezvous under " << name
      << ". §5.8's form 3 exists to need no memfd, no lock file and no participant slot.";
  }

  // 2. The same node, plus the parameter.
  {
    auto node = std::make_shared<tf_tree_ros::BridgeNode>(
      with({rclcpp::Parameter("arena_name", name)}));

    tft_tree * tree = open_within(10s);
    ASSERT_NE(tree, nullptr)
      << "no rendezvous appeared under " << name << " within 10 s, so the arena_name parameter "
      << "reached no further than the node. $TF_TREE_RUNTIME_DIR=" << scratch_dir();

    expect_the_topology_is_there(tree);
    tft_tree_free(tree);
  }
}

/// §5.8's **form 3** — a caller that owns its own node and fills
/// `BridgeOptions` directly — reaches the same arena.
///
/// Forms 1 and 2 are `BridgeNode` and inherit the parameter; form 3 never
/// constructs one and has no parameters at all, so the field is its whole
/// surface. This is the assertion that the two halves are the same field: the
/// test above could pass with `BridgeNode` doing something private that form 3
/// could not do.
///
/// **Mutant:** in `BridgeHandle::create_bridge`, delete the
/// `o.arena_name = ...` line, so the field stays the `{}`-initialised nullptr
/// and no caller of any form can ask for a shared arena. This dies, and **so
/// does the test above** — that is the point of stating it here: the two tests
/// share the `BridgeOptions`-to-ABI half and differ only in how the field is
/// filled, so this mutant is the one that is *not* specific to either. Applied;
/// both die.
TEST(SharedArenaTest, form_3_publishes_the_arena_through_the_options_field)
{
  const std::string name = arena_name("form3");
  ASSERT_EQ(::setenv("TF_TREE_NAME", name.c_str(), 1), 0);

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

  expect_the_topology_is_there(tree);
  tft_tree_free(tree);
}

/// A second bridge on a name the first already holds refuses to start, and says
/// so through this package's error type rather than by joining an arena it did
/// not size.
///
/// The refusal itself is the C ABI's and `bridge_shared.rs` pins its status and
/// its message. What is only checkable here is that it survives the two layers
/// above it — that `BridgeHandle`'s constructor turns a failing
/// `tft_bridge_create` into a `BridgeError` instead of a half-built handle, on
/// the ingest thread, where the exception has to cross a `std::promise` to be
/// seen at all.
///
/// **Mutant:** in `BridgeHandle::run`, publish `TFT_OK` through the promise
/// regardless of what `create_bridge()` returned. Nothing throws and
/// `EXPECT_THROW` fails. (`test_ingest.cpp` carries the same mutant against a
/// bad config; this is the arena-ownership path reaching the same code.)
TEST(SharedArenaTest, a_second_bridge_on_a_held_name_refuses_to_start)
{
  const std::string name = arena_name("held");
  ASSERT_EQ(::setenv("TF_TREE_NAME", name.c_str(), 1), 0);

  auto node = std::make_shared<rclcpp::Node>("tf_tree_shared_arena_held");
  tf_tree_ros::BridgeOptions o;
  o.topology_toml = kTopology;
  o.tf_topic = "/tf_shared_arena_held";
  o.tf_static_topic = "/tf_shared_arena_held_static";
  o.arena_name = name;
  tf_tree_ros::BridgeHandle first(node.get(), o);

  auto second_node = std::make_shared<rclcpp::Node>("tf_tree_shared_arena_held_2");
  EXPECT_THROW(tf_tree_ros::BridgeHandle(second_node.get(), o), tf_tree_ros::BridgeError);

  // And the first is still the one serving: a refusal that tore down the
  // incumbent's rendezvous would be worse than one that joined.
  tft_tree * tree = open_within(10s);
  ASSERT_NE(tree, nullptr) << "the refused second bridge took the first one's arena with it";
  expect_the_topology_is_there(tree);
  tft_tree_free(tree);
}

}  // namespace

int main(int argc, char ** argv)
{
  // **Before `rclcpp::init` and before any test**, because the ingest thread
  // reads these when it creates the bridge and `setenv` is process-wide.
  // `$TF_TREE_DOMAIN` is pinned rather than inherited: it falls back to
  // `$ROS_DOMAIN_ID`, which colcon and a developer's shell both set, and a
  // rendezvous that moved with it would make this test's isolation depend on
  // somebody else's environment.
  const std::string dir = scratch_dir();
  std::filesystem::remove_all(dir);
  ::setenv("TF_TREE_RUNTIME_DIR", dir.c_str(), 1);
  ::setenv("TF_TREE_DOMAIN", "0", 1);

  ::testing::InitGoogleTest(&argc, argv);
  rclcpp::init(argc, argv);
  const int rc = RUN_ALL_TESTS();
  rclcpp::shutdown();

  std::error_code ignored;
  std::filesystem::remove_all(dir, ignored);
  return rc;
}
