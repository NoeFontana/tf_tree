# tf_tree_bench_ros — the `docs/PHASE5.md` §9.1 end-to-end comparison.
#
# A separate package from `tf_tree_ros` on purpose: that one is shipped and
# installed on a robot, this one is a benchmark and must never be. Splitting
# them also keeps `tf2_ros` — which the bridge deliberately does not depend on —
# out of the shipped package's dependency set.
#
# It reaches the engine the same way `tf_tree_ros` does, through
# `find_package(tf_tree CONFIG)`, and it reaches the bridge through
# `find_package(tf_tree_ros)`. Nothing here includes a private header of either.

cmake_minimum_required(VERSION 3.16)
# Version: a hand-kept copy of the root `Cargo.toml`'s `[workspace.package]
# version` — see `tf_tree_ros/CMakeLists.txt` for why a benchmark-only package
# still carries the same number.
project(tf_tree_bench_ros VERSION 0.0.3 LANGUAGES CXX)

if(NOT CMAKE_CXX_STANDARD)
  set(CMAKE_CXX_STANDARD 17)
endif()
add_compile_options(-Wall -Wextra -Wpedantic)

find_package(ament_cmake REQUIRED)
find_package(rclcpp REQUIRED)
find_package(geometry_msgs REQUIRED)
find_package(tf2_msgs REQUIRED)
find_package(tf2_ros REQUIRED)
find_package(tf_tree CONFIG REQUIRED)
find_package(tf_tree_ros REQUIRED)

# The one publisher both arms listen to. §9.3's "identical data" is structural
# here rather than promised: there is only one process producing traffic.
add_executable(tf_publisher src/tf_publisher.cpp)
target_link_libraries(tf_publisher
  rclcpp::rclcpp
  ${geometry_msgs_TARGETS}
  ${tf2_msgs_TARGETS})

# One consumer, either engine, selected by `--mode`. Sharing one executable
# between the arms is what keeps the schedule, the query set, the warm-up window
# and the measurement code identical between them — the parts §9.3 says must not
# differ are the parts that are literally the same code.
add_executable(bench_consumer src/bench_consumer.cpp)
target_include_directories(bench_consumer PRIVATE
  "$<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>")
target_link_libraries(bench_consumer
  rclcpp::rclcpp
  tf2_ros::tf2_ros
  tf_tree_ros::tf_tree_ros
  tf_tree::tf_tree_static
  ${geometry_msgs_TARGETS}
  ${tf2_msgs_TARGETS})

install(TARGETS tf_publisher bench_consumer
  RUNTIME DESTINATION lib/${PROJECT_NAME})

ament_package()
