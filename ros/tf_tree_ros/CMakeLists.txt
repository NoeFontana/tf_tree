# tf_tree_ros — the ROS 2 ingest bridge, `docs/PHASE4.md` §5.
#
# This package is deliberately NOT a cargo crate and not under `crates/`: it
# needs `rclcpp`, which exists only inside `docker/tf2`, and `tf_tree_tf2_sys`
# already records what happens to a crate the workspace cannot see — no `cargo
# fmt`, no `clippy`, and unit tests that run nowhere. Everything that does not
# strictly need a middleware lives in `crates/tf_tree_bridge` for exactly that
# reason, and what is left here is subscriptions, QoS and lifecycle.
#
# It reaches tf_tree only through `find_package(tf_tree CONFIG)` — the same
# entry point `just cmake-check` proves for an ordinary C++ consumer.

cmake_minimum_required(VERSION 3.16)
project(tf_tree_ros VERSION 0.1.0 LANGUAGES CXX)

if(NOT CMAKE_CXX_STANDARD)
  set(CMAKE_CXX_STANDARD 17)
endif()
add_compile_options(-Wall -Wextra -Wpedantic)

find_package(ament_cmake REQUIRED)
find_package(rclcpp REQUIRED)
find_package(rclcpp_components REQUIRED)
find_package(geometry_msgs REQUIRED)
find_package(tf2_msgs REQUIRED)

# **`tf_tree::tf_tree_static`, not `tf_tree::tf_tree`.** The shared library is a
# rustc cdylib with no `DT_SONAME`, so linking it records the build-time
# absolute path in this package's `DT_NEEDED` and the colcon install tree cannot
# be moved. Linking the staticlib into our own shared library is relocatable and
# gives the process exactly one copy of the arena's globals.
#
# The staticlib must have been built `--features bridge`, or none of the nine §5
# entry points is in it and this fails at link time with a list of missing
# `tft_bridge_*` symbols. `just ros-build` builds it that way.
find_package(tf_tree CONFIG REQUIRED)

add_library(tf_tree_ros SHARED src/bridge_handle.cpp src/bridge_node.cpp)
target_include_directories(tf_tree_ros PUBLIC
  "$<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>"
  "$<INSTALL_INTERFACE:include>")
# Modern target form rather than `ament_target_dependencies`, which is
# deprecated from ament_cmake 2.x and warns under `-Werror`-ish CI settings.
# `${<pkg>_TARGETS}` is what the generated message packages export.
target_link_libraries(tf_tree_ros PUBLIC
  tf_tree::tf_tree_static
  rclcpp::rclcpp
  ${geometry_msgs_TARGETS}
  ${tf2_msgs_TARGETS})

# §5.8 forms 2 and 1. **One `rclcpp_components_register_node` call produces
# both**: the loadable plugin, and a standalone executable named `tf_tree_bridge`
# that spins it. The component library is separate from `tf_tree_ros` so a
# consumer using form 3 does not drag in `rclcpp_components` and the plugin
# registration.
add_library(tf_tree_ros_component SHARED src/bridge_component.cpp)
target_link_libraries(tf_tree_ros_component PUBLIC
  tf_tree_ros
  rclcpp_components::component)
rclcpp_components_register_node(tf_tree_ros_component
  PLUGIN "tf_tree_ros::BridgeNode"
  EXECUTABLE tf_tree_bridge)

install(DIRECTORY include/ DESTINATION include)
install(TARGETS tf_tree_ros tf_tree_ros_component EXPORT export_tf_tree_ros
  ARCHIVE DESTINATION lib
  LIBRARY DESTINATION lib
  RUNTIME DESTINATION bin)

ament_export_targets(export_tf_tree_ros HAS_LIBRARY_TARGET)
ament_export_dependencies(rclcpp geometry_msgs tf2_msgs)

if(BUILD_TESTING)
  find_package(ament_cmake_gtest REQUIRED)
  # Test-only, and only for `test_clock`: publishing `/clock` is how a test
  # makes a `TimeSource` jump. Nothing in the library depends on the message
  # type — it asks `rclcpp::Clock`, which is the seam that exists for this.
  find_package(rosgraph_msgs REQUIRED)

  # These need a real DDS and a second participant, so they are `ctest`s rather
  # than unit tests — and `just ros-test` is the only gate they run in.
  ament_add_gtest(test_ingest test/test_ingest.cpp TIMEOUT 240)
  if(TARGET test_ingest)
    target_link_libraries(test_ingest tf_tree_ros)
  endif()

  # §6.3's QoS regression, as amended. Separate from `test_ingest` because it
  # deliberately waits out timeouts — one of its two assertions is that nothing
  # arrives — and mixing that with the fast path makes the fast path look slow.
  ament_add_gtest(test_qos test/test_qos.cpp TIMEOUT 120)
  if(TARGET test_qos)
    target_link_libraries(test_qos tf_tree_ros)
  endif()

  # §5.3 attribution and §5.4's diagnostic. The one thing here that cannot be
  # checked anywhere else is that `MessageInfo::publisher_gid` and the graph's
  # `endpoint_gid()` are the same 16 bytes on a real RMW.
  ament_add_gtest(test_attribution test/test_attribution.cpp TIMEOUT 180)
  if(TARGET test_attribution)
    target_link_libraries(test_attribution tf_tree_ros)
  endif()

  # §5.8 forms 1 and 2: the parameter surface, which is all they add to form 3.
  ament_add_gtest(test_node test/test_node.cpp TIMEOUT 120)
  if(TARGET test_node)
    target_link_libraries(test_node tf_tree_ros)
  endif()

  # §5.5's authoritative jump path, and the clock every diagnostic in
  # `report()` is rate-limited on. **Its own binary because it is the only test
  # in this package that turns `use_sim_time` on and publishes `/clock`**, which
  # changes what "now" means for every node in the process — including the ones
  # the other four suites build.
  ament_add_gtest(test_clock test/test_clock.cpp TIMEOUT 180)
  if(TARGET test_clock)
    target_link_libraries(test_clock tf_tree_ros ${rosgraph_msgs_TARGETS})
  endif()

  # `docs/decisions/0015`: the `arena_name` parameter, and the attach it exists
  # to make possible. **Its own binary because it is the only test here that
  # rewrites `$TF_TREE_RUNTIME_DIR`, `$TF_TREE_DOMAIN` and `$TF_TREE_NAME`** —
  # process-wide state that selects which arena *any* `tft_tree_open` in the
  # process resolves to, set once in its `main` before `rclcpp::init`. Sharing a
  # binary with a suite that does not expect it would make that suite's arena
  # depend on test ordering.
  ament_add_gtest(test_shared_arena test/test_shared_arena.cpp TIMEOUT 180)
  if(TARGET test_shared_arena)
    target_link_libraries(test_shared_arena tf_tree_ros)
  endif()

  # §5.8's amendment: form 3 attached to a node the caller is already spinning.
  # Its own binary because the mutant it exists for kills the *process* rather
  # than failing an assertion — `add_callback_group` throws on the ingest
  # thread, where nothing catches it — and a crash must not take the other
  # tests' results with it.
  ament_add_gtest(test_executor test/test_executor.cpp TIMEOUT 120)
  if(TARGET test_executor)
    target_link_libraries(test_executor tf_tree_ros)
  endif()
endif()

ament_package()
