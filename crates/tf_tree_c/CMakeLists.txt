# tf_tree — CMake package for the C ABI and the C++ wrapper. docs/PHASE4.md §4.4.
#
# Exports two targets:
#
#   tf_tree::tf_tree          the shared library (cdylib)
#   tf_tree::tf_tree_static   the static library (staticlib)
#
# Both carry the headers and `cxx_std_17` as INTERFACE properties, so a consumer
# writes `target_link_libraries(mynode PRIVATE tf_tree::tf_tree)` and is done.
#
# # Prebuilt is preferred over building from source, and that is the point
#
# §4.4: "Requiring a Rust toolchain is real adoption friction." A ROS shop that
# wants to try tf_tree should not have to install rustup first. So:
#
#   1. If TF_TREE_PREBUILT_DIR holds a library for this target, use it.
#   2. Otherwise, if `corrosion` is available, build from source with it, so a
#      `colcon build` works without the user hand-managing cargo.
#   3. Otherwise, invoke cargo directly through a custom command.
#
# Step 3 exists because corrosion is itself a dependency to install, and the
# fallback should not be "read the manual".

cmake_minimum_required(VERSION 3.16)
project(tf_tree VERSION 0.1.0 LANGUAGES C CXX)

include(GNUInstallDirs)
include(CMakePackageConfigHelpers)

set(TF_TREE_INCLUDE_DIR "${CMAKE_CURRENT_SOURCE_DIR}/include")

option(TF_TREE_BUILD_FROM_SOURCE "Build the Rust library with cargo" ON)
set(TF_TREE_PREBUILT_DIR "" CACHE PATH
    "Directory holding a prebuilt libtf_tree_c.{a,so} for this target")

# --- where the library comes from ------------------------------------------

# **Do not pre-set these**, and do not give both `find_library` calls the same
# NAMES. Two separate defects lived in four lines here:
#
#  * `set(_tf_tree_static "")` before `find_library(_tf_tree_static ...)` makes
#    the search a no-op — `find_library` skips entirely when a variable of that
#    name is already defined. `TF_TREE_PREBUILT_DIR` was dead code.
#  * both calls used `NAMES tf_tree_c`, and `CMAKE_FIND_LIBRARY_SUFFIXES` is
#    `.so;.a`, so both resolved to the *same* `.so` — the "static" target was a
#    shared library wearing a static label.
#
# Distinct cache variables, distinct NAMES, and `unset` rather than `set ""`.
unset(_tf_tree_static)
unset(_tf_tree_shared)
set(_tf_tree_corrosion FALSE)

if(TF_TREE_PREBUILT_DIR)
  find_library(TF_TREE_PREBUILT_STATIC NAMES libtf_tree_c.a
               PATHS "${TF_TREE_PREBUILT_DIR}" NO_DEFAULT_PATH)
  find_library(TF_TREE_PREBUILT_SHARED NAMES libtf_tree_c.so
               PATHS "${TF_TREE_PREBUILT_DIR}" NO_DEFAULT_PATH)
  if(TF_TREE_PREBUILT_STATIC OR TF_TREE_PREBUILT_SHARED)
    set(_tf_tree_static "${TF_TREE_PREBUILT_STATIC}")
    set(_tf_tree_shared "${TF_TREE_PREBUILT_SHARED}")
    message(STATUS "tf_tree: using prebuilt from ${TF_TREE_PREBUILT_DIR}")
  else()
    message(WARNING
        "tf_tree: TF_TREE_PREBUILT_DIR=${TF_TREE_PREBUILT_DIR} holds no "
        "libtf_tree_c.a or libtf_tree_c.so; falling back to a source build")
  endif()
endif()

if(NOT _tf_tree_static AND TF_TREE_BUILD_FROM_SOURCE)
  find_package(Corrosion QUIET)
  if(Corrosion_FOUND)
    message(STATUS "tf_tree: building from source via corrosion")
    corrosion_import_crate(MANIFEST_PATH "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml"
                           PROFILE release)
    set(_tf_tree_corrosion TRUE)
    # **Export the same target names corrosion does not.** Without this the
    # corrosion branch imported the crate and then exported nothing: no
    # `tf_tree::tf_tree`, no `tf_tree::tf_tree_static`, and no install rule for
    # the library — so `find_package(tf_tree CONFIG)` found headers and a config
    # file describing libraries that were never installed.
    add_library(tf_tree::tf_tree ALIAS tf_tree_c-shared)
    add_library(tf_tree::tf_tree_static ALIAS tf_tree_c-static)
    foreach(_t tf_tree_c-shared tf_tree_c-static)
      target_include_directories(${_t} INTERFACE "${TF_TREE_INCLUDE_DIR}")
      target_compile_features(${_t} INTERFACE cxx_std_17)
    endforeach()
    install(IMPORTED_RUNTIME_ARTIFACTS tf_tree_c-shared
            LIBRARY DESTINATION "${CMAKE_INSTALL_LIBDIR}" OPTIONAL)
  else()
    message(STATUS "tf_tree: building from source via cargo (corrosion not found)")
    set(_cargo_out "${CMAKE_CURRENT_BINARY_DIR}/cargo")
    set(_tf_tree_static "${_cargo_out}/release/libtf_tree_c.a")
    set(_tf_tree_shared "${_cargo_out}/release/libtf_tree_c.so")
    add_custom_command(
      OUTPUT "${_tf_tree_static}" "${_tf_tree_shared}"
      # `-Wl,-soname` is load-bearing for a relocatable install. A rustc cdylib
      # carries no `DT_SONAME`, so a consumer linking `tf_tree::tf_tree` records
      # the **absolute build-time path** in its `DT_NEEDED` and the installed
      # prefix cannot be moved — or packaged.
      COMMAND ${CMAKE_COMMAND} -E env
              "RUSTFLAGS=-C link-arg=-Wl,-soname,libtf_tree_c.so"
              cargo build --release
              --manifest-path "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml"
              --target-dir "${_cargo_out}"
      WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}"
      COMMENT "Building tf_tree_c with cargo"
      VERBATIM)
    # `ALL`, and it is load-bearing: the two library targets below are
    # IMPORTED, so `add_dependencies` on them only fires when something in
    # *this* project links them — and nothing does. Without `ALL`,
    # `cmake --build .` built nothing at all and `cmake --install .` quietly
    # installed three headers and no library.
    add_custom_target(tf_tree_cargo ALL DEPENDS "${_tf_tree_static}" "${_tf_tree_shared}")
  endif()
endif()

# --- the exported targets ---------------------------------------------------

if(NOT _tf_tree_corrosion)
  add_library(tf_tree_static STATIC IMPORTED GLOBAL)
  set_target_properties(tf_tree_static PROPERTIES
      IMPORTED_LOCATION "${_tf_tree_static}"
      INTERFACE_INCLUDE_DIRECTORIES "${TF_TREE_INCLUDE_DIR}")
  target_compile_features(tf_tree_static INTERFACE cxx_std_17)
  # A Rust staticlib does not carry its own system dependencies.
  set_property(TARGET tf_tree_static APPEND PROPERTY
      INTERFACE_LINK_LIBRARIES pthread dl m)
  if(TARGET tf_tree_cargo)
    add_dependencies(tf_tree_static tf_tree_cargo)
  endif()

  add_library(tf_tree_shared SHARED IMPORTED GLOBAL)
  set_target_properties(tf_tree_shared PROPERTIES
      IMPORTED_LOCATION "${_tf_tree_shared}"
      INTERFACE_INCLUDE_DIRECTORIES "${TF_TREE_INCLUDE_DIR}")
  target_compile_features(tf_tree_shared INTERFACE cxx_std_17)
  if(TARGET tf_tree_cargo)
    add_dependencies(tf_tree_shared tf_tree_cargo)
  endif()

  add_library(tf_tree::tf_tree ALIAS tf_tree_shared)
  add_library(tf_tree::tf_tree_static ALIAS tf_tree_static)
endif()

# --- installation and find_package(tf_tree CONFIG) --------------------------

install(FILES
    "${TF_TREE_INCLUDE_DIR}/tf_tree.h"
    "${TF_TREE_INCLUDE_DIR}/tf_tree_unstable.h"
    "${TF_TREE_INCLUDE_DIR}/tf_tree.hpp"
    DESTINATION "${CMAKE_INSTALL_INCLUDEDIR}")

# `OPTIONAL`, and **no `EXISTS` guard**: `EXISTS` is evaluated at *configure*
# time, when cargo has not run yet, so guarding on it installed the headers and
# silently nothing else. `OPTIONAL` is the right tool — it defers the question
# to install time and tolerates a configuration where only one of the two was
# produced.
if(_tf_tree_static)
  install(FILES "${_tf_tree_static}" DESTINATION "${CMAKE_INSTALL_LIBDIR}" OPTIONAL)
endif()
if(_tf_tree_shared)
  install(FILES "${_tf_tree_shared}" DESTINATION "${CMAKE_INSTALL_LIBDIR}" OPTIONAL)
endif()

# `PATH_VARS` is required, not optional: `configure_package_config_file` only
# rewrites `@PACKAGE_<var>@` for variables named here. Without it the config
# file shipped a literal empty include directory and `find_package` failed with
# "File or directory referenced by variable TF_TREE_INCLUDE_DIR does not exist"
# in the consumer, not here — which is the wrong place to find out.
configure_package_config_file(
    "${CMAKE_CURRENT_SOURCE_DIR}/cmake/tf_treeConfig.cmake.in"
    "${CMAKE_CURRENT_BINARY_DIR}/tf_treeConfig.cmake"
    INSTALL_DESTINATION "${CMAKE_INSTALL_LIBDIR}/cmake/tf_tree"
    PATH_VARS CMAKE_INSTALL_INCLUDEDIR CMAKE_INSTALL_LIBDIR)
write_basic_package_version_file(
    "${CMAKE_CURRENT_BINARY_DIR}/tf_treeConfigVersion.cmake"
    VERSION ${PROJECT_VERSION}
    COMPATIBILITY SameMajorVersion)
install(FILES
    "${CMAKE_CURRENT_BINARY_DIR}/tf_treeConfig.cmake"
    "${CMAKE_CURRENT_BINARY_DIR}/tf_treeConfigVersion.cmake"
    DESTINATION "${CMAKE_INSTALL_LIBDIR}/cmake/tf_tree")
