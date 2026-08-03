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

/// A runtime directory nobody else can be using, created once by `main` and
/// remembered here.
///
/// **Isolation here is by directory, not by name.** The rendezvous is selected
/// by `(runtime dir, domain, name)`. The ctests in this package do *not* run in
/// parallel — `colcon test` invokes ctest without `-j` and the run is strictly
/// sequential — so the thing this defends against is not a sibling suite; it is
/// every *other process* on the machine: a developer's `tf_tree serve`, a
/// previous run of this binary that a signal killed before its cleanup, a robot.
/// A unique `$TF_TREE_RUNTIME_DIR` makes all of those unreachable from this
/// process and this process unreachable from them, whatever names collide.
///
/// **`mkdtemp`, not the pid.** A pid is reused, and the loser of that race is
/// this test reading a directory somebody else's dead run left behind — which is
/// precisely the state the isolation exists to rule out, arrived at through the
/// mechanism meant to prevent it.
const std::string & scratch_dir()
{
  static const std::string dir = [] {
      std::string tmpl = "/tmp/tf_tree_ros_shared-XXXXXX";
      if (::mkdtemp(tmpl.data()) == nullptr) {
        // `main` calls this before `InitGoogleTest`, so there is no test to fail
        // and no reporter to fail it into. Nothing this suite asserts means
        // anything without an isolated runtime directory.
        std::perror("mkdtemp(/tmp/tf_tree_ros_shared-XXXXXX)");
        std::abort();
      }
      return tmpl;
    }();
  return dir;
}

/// **One arena name for the whole binary**, set into `$TF_TREE_NAME` by `main`.
///
/// Every test here creates its bridge under this name and destroys it before
/// returning, and the run is sequential (see `scratch_dir`), so one name is
/// enough. One name also makes the negative half of
/// `the_arena_name_parameter_is_what_a_separate_attach_finds` mean something
/// beyond itself: it asserts that nothing is live under this name at that point
/// in the run, so a test that leaked its bridge fails *there*, naming the arena,
/// rather than leaving whatever runs next to fail on a name it could not create.
/// gtest runs tests in declaration order and that one is declared first, so its
/// own negative half cannot be polluted by the positive halves below it — which
/// is the requirement — and the leak assertion covers whatever is added above it.
///
/// Inside `tf_tree_ipc`'s 64-byte limit and a single path component.
std::string arena_name()
{
  return "rosarena-" + std::to_string(::getpid());
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

/// `tft_tree_open` once, **returning its status** — the negative half, which
/// must not spend a timeout proving a rendezvous that was never published is
/// absent.
///
/// The status is returned rather than reduced to a bool so the caller can say
/// *which* failure it expects. "The open failed" is satisfied by a bad handle
/// argument, an ABI mismatch, or a fork-poisoned process, none of which is
/// evidence that no arena was published. `tft_tree_open` collapses every join
/// failure onto `TFT_ERR_INTERNAL` — the collapse `docs/decisions/0015`'s
/// *Failure* section describes and gave `tft_bridge_create` a code of its own to
/// escape — so "arena absent" and "runtime directory unusable" are still one
/// code here and the assertion cannot separate them. What it does separate is
/// that code from every *other* way this call can fail, which is where the
/// negative half was previously satisfiable for free.
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
/// Two `BridgeNode`s differing in one parameter, against the one name in
/// `$TF_TREE_NAME`, and the negative first so it cannot be polluted by the
/// positive. Without `arena_name` nothing is reachable under that name; with it,
/// the attach succeeds and sees the topology.
///
/// Running both halves against **one** name is the whole design. A negative
/// half that used a different name would assert only that an unused name is
/// unused, which is true of any implementation whatsoever.
///
/// **Declared first, deliberately** — see `arena_name()`. Its negative half is
/// the file's leak assertion, and a leak assertion is worth nothing if the
/// thing it would catch has already made the positive halves fail.
///
/// **Mutant:** in `BridgeNode`'s constructor, ignore the parameter — read it
/// and then overwrite it, `o.arena_name = "";`. That is the node-layer half of
/// the wiring, the half `form_3_publishes_the_arena_through_the_options_field`
/// below cannot see, and it is the failure this test exists for: the node
/// constructs, the bridge runs, and every other test in this package passes.
/// Applied; it dies.
TEST(SharedArenaTest, the_arena_name_parameter_is_what_a_separate_attach_finds)
{
  const std::string name = arena_name();

  // 1. Today's default: no `arena_name` at all. The arena is private to the
  //    bridge's own process and there is nothing under the name to find.
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
/// The refusal itself is the C ABI's and `bridge_shared.rs` pins its status and
/// its message. What is only checkable here is that it survives the two layers
/// above it: `create_bridge()`'s `tft_status` is what crosses the `std::promise`
/// out of the ingest thread (`bridge_handle.cpp`'s `run`), and the constructor
/// — on the *calling* thread, having joined — is what turns a non-`TFT_OK` into
/// a `BridgeError` instead of returning a half-built handle. Neither half is
/// visible from the ABI's own tests, and the exception never crosses a thread
/// boundary; the status does.
///
/// **The assertion is on the code, not the type.** `docs/API.md` §1 R5 makes the
/// `tft_status` the contract and the message a diagnostic, and
/// `docs/decisions/0015`'s *Failure* section spends four paragraphs arguing that
/// "another bridge holds this name" had to be distinguishable from "the runtime
/// directory is unusable" and from a bug — which is what
/// `TFT_ERR_ARENA_UNAVAILABLE` exists for. A test that pinned only the C++
/// exception *type* let that whole argument be undone for free.
///
/// **Mutant:** in `BridgeHandle::run`, publish `TFT_OK` through the promise
/// regardless of what `create_bridge()` returned. Nothing throws and the
/// `FAIL()` below fires. Applied; it dies. (`test_ingest.cpp` carries the same
/// mutant against a bad config, and dies too; this is the arena-ownership path
/// reaching the same code.)
///
/// **Mutant:** in `crates/tf_tree_c/src/bridge.rs`, collapse the shared-arena
/// refusal back onto `TFT_ERR_INTERNAL` — `fn arena_unavailable` is the one
/// funnel every such refusal goes through. A `BridgeError` still crosses the
/// promise and is still caught, so a type-only assertion survives this
/// untouched; `e.status()` does not. Applied; it dies, reporting the message
/// (which is unchanged, and is the point: R5's diagnostic was never the thing
/// that identified the fault).
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

  // And the first is still the one serving: a refusal that tore down the
  // incumbent's rendezvous would be worse than one that joined.
  tft_tree * tree = open_within(10s);
  ASSERT_NE(tree, nullptr) << "the refused second bridge took the first one's arena with it";
  ASSERT_NO_FATAL_FAILURE(expect_the_topology_is_there(tree));
  tft_tree_free(tree);
}

}  // namespace

int main(int argc, char ** argv)
{
  // **All three of these, before `rclcpp::init` and before any test**, because
  // the ingest thread reads them when it creates the bridge and `setenv` is
  // process-wide. `setenv` after `rclcpp::init` races every `getenv` in every
  // rclcpp and RMW thread the init spawned, which is undefined behaviour and not
  // the kind that announces itself; `TF_TREE_NAME` used to be set from inside
  // three `TEST` bodies, which is exactly that. There is one name (see
  // `arena_name()`) precisely so that it can be set here with the other two.
  //
  // `$TF_TREE_DOMAIN` is pinned rather than inherited: it falls back to
  // `$ROS_DOMAIN_ID`, which colcon and a developer's shell both set, and a
  // rendezvous that moved with it would make this test's isolation depend on
  // somebody else's environment.
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
