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
# The version here is a hand-kept copy of `[workspace.package] version` in the
# root `Cargo.toml`, which carries the argument for the number. It is the
# version a consumer's `find_package(tf_tree 0.0.1 CONFIG REQUIRED)` matches
# against, so it must not drift from the library it is describing — and it is
# **not** the ABI version, which `TFT_ABI_VERSION_MAJOR`/`_MINOR` carry (0.5 as
# of this commit) and `docs/PHASE4.md` §3 versions independently of the crate.
project(tf_tree VERSION 0.0.4 LANGUAGES C CXX)

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

# --- does each library have `shm` in it? ------------------------------------

# `tf_tree.h` hides `tft_tree_open` behind `#if defined(TFT_HAVE_SHM)` and
# **nothing defined it**, so the only way a consumer reached the shared-arena
# entry point was to hand-type the macro — against an archive that may well have
# been built without `--features shm`. That compiles and then fails at link, in
# the consumer, naming a symbol the consumer never wrote. The macro has to come
# from the artifact.
#
# So: ask each resolved library whether it defines `tft_tree_open`, and put
# `TFT_HAVE_SHM=1` on that library's exported target only when it does.
#
# **One probe per artifact, and the "per artifact" is the whole of it.** This
# used to read *one* library — the `.a` if there was one, else the `.so` — and
# apply that single answer to both exported targets, on the stated argument that
# they are "two builds of one crate". That argument is sound for `cargo build`,
# which produces both from one feature set, and it is exactly what the prebuilt
# path does not guarantee: `TF_TREE_PREBUILT_DIR` promises only "a directory
# holding a prebuilt libtf_tree_c.{a,so} for this target", and a directory
# holding a `bridge,shm` archive beside a `bridge`-only shared library satisfies
# that promise. The config then declared `TFT_HAVE_SHM=1` for both, and a
# consumer linking `tf_tree::tf_tree` got `undefined reference to
# 'tft_tree_open'` — the exact link error this section exists to turn into a
# compile error. Two `find_library` calls already resolve the two libraries
# independently; the probe now answers independently too.
#
# **Off is the safe answer, and that is why every uncertain case takes it.** With
# the definition off a consumer that calls `tft_tree_open` gets a *compile*
# error naming that identifier; with it wrongly on, a link error naming a mangled
# symbol. The cases that cannot be probed, each of which takes off:
#
#   * corrosion (`_tf_tree_corrosion`): the library does not exist at configure
#     time and CMake does not choose its feature set — `corrosion_import_crate`
#     is called with no `FEATURES`, so cargo's defaults apply and `shm` is
#     default-off. Off here is therefore *accurate*, not merely conservative; if
#     this branch ever grows a `FEATURES` argument, this is the line that has to
#     grow with it.
#   * the cargo-fallback branch: same, one step further out — the path in
#     `_tf_tree_static` is a promise, not a file, until `cmake --build` runs, and
#     that build is a plain `cargo build --release` with default features.
#   * no `nm`: nothing to ask.
#   * an artifact that is simply not there — a prebuilt directory with only one
#     of the two. Its target is not exported either, so the answer is moot, but
#     it is answered rather than inherited from the other one.
#
# A further case used to hide *silently* in the pattern itself, which is worse
# than any of the four above: a platform whose `nm` prefixes C symbols with an
# underscore (Darwin prints ` T _tft_tree_open`) matched nothing, took off, and
# said so nowhere — no message, no line in this list, and an `shm` build
# reported as one without it. The `_?` in the pattern below is why it is not a
# case any more.
#
# In practice the probe fires for `TF_TREE_PREBUILT_DIR`, which is the path
# `ros/build.sh` and every packaged deployment take.

# `CMAKE_NM` is set by CMakeFindBinUtils for the enabled languages; a toolchain
# file may override it, and honouring that is the point of looking there first.
set(_tf_tree_nm "${CMAKE_NM}")
if(NOT _tf_tree_nm)
  find_program(TF_TREE_NM NAMES nm)
  set(_tf_tree_nm "${TF_TREE_NM}")
endif()

# Ask one artifact whether `tft_tree_open` is defined in it, and set `_out` to 1
# or 0 in the caller's scope. Every message names the library it answered for,
# because the two answers can differ and a log line that did not say which one
# it was about would be the same defect one layer up.
function(tf_tree_probe_shm _lib _out)
  set(${_out} 0 PARENT_SCOPE)
  if(NOT _lib)
    return()
  endif()
  if(NOT EXISTS "${_lib}")
    message(STATUS "tf_tree: ${_lib} does not exist at configure time; TFT_HAVE_SHM off for it")
    return()
  endif()
  if(NOT _tf_tree_nm)
    message(STATUS
        "tf_tree: no nm found, cannot tell whether ${_lib} has shm in it; "
        "TFT_HAVE_SHM off for it. A consumer calling tft_tree_open will fail to "
        "compile rather than fail to link.")
    return()
  endif()
  # **Two tables, and a shared library only reliably has the second.**
  # `nm --defined-only` reads the *static* symbol table, which `strip` removes;
  # `nm -D` reads the *dynamic* one, which it cannot, because that is the table
  # the loader resolves against. An archive has only the static one, so the
  # plain form is tried first and `-D` is the fallback rather than the reverse.
  #
  # Measured on a release `libtf_tree_c.so`, grepping for `tft_tree_open`:
  #
  #   unstripped   nm -> 1   nm -D -> 1
  #   stripped     nm -> 0   nm -D -> 1
  #
  # Reading only the static table made this probe answer "built without
  # --features shm" for a shm build whose `.so` had been stripped — a false
  # negative that a consumer meets as `tft_tree_open` being undeclared, with a
  # message blaming a feature flag they did set. It reached a CI runner and not
  # a developer machine, because `[profile.release] strip = "debuginfo"` leaves
  # the symtab intact here and something in that toolchain did not.
  execute_process(
      COMMAND "${_tf_tree_nm}" --defined-only "${_lib}"
      OUTPUT_VARIABLE _syms
      ERROR_VARIABLE _nm_err
      RESULT_VARIABLE _nm_rc)
  # A stripped shared library makes `nm` fail outright on some binutils and
  # return an empty list on others, so both are retried the same way.
  if(NOT _nm_rc EQUAL 0 OR NOT _syms MATCHES " T _?tft_tree_open[\r\n]")
    execute_process(
        COMMAND "${_tf_tree_nm}" -D --defined-only "${_lib}"
        OUTPUT_VARIABLE _dyn_syms
        ERROR_VARIABLE _dyn_err
        RESULT_VARIABLE _dyn_rc)
    if(_dyn_rc EQUAL 0 AND _dyn_syms MATCHES " T _?tft_tree_open[\r\n]")
      set(_syms "${_dyn_syms}")
      set(_nm_rc 0)
      message(STATUS "tf_tree: ${_lib} answered from the dynamic symbol table")
    endif()
  endif()
  if(NOT _nm_rc EQUAL 0)
    # `_nm_err` is the whole point of capturing it: "nm failed" without the
    # reason sends the reader to run it by hand to find out.
    message(STATUS
        "tf_tree: ${_tf_tree_nm} failed on ${_lib} (exit ${_nm_rc}): ${_nm_err}; "
        "TFT_HAVE_SHM off for it")
    return()
  endif()
  # The trailing newline class matters: it is what keeps this from matching a
  # hypothetical `tft_tree_open_v2`, which would be exactly the wrong answer.
  # The optional underscore is Darwin's `nm`, which prefixes every C symbol.
  if(_syms MATCHES " T _?tft_tree_open[\r\n]")
    set(${_out} 1 PARENT_SCOPE)
    message(STATUS "tf_tree: ${_lib} defines tft_tree_open; TFT_HAVE_SHM=1 for it")
  else()
    message(STATUS
        "tf_tree: ${_lib} does not define tft_tree_open (built without "
        "--features shm); TFT_HAVE_SHM off for it")
  endif()
endfunction()

set(TF_TREE_HAVE_SHM_STATIC 0)
set(TF_TREE_HAVE_SHM_SHARED 0)

if(_tf_tree_corrosion)
  message(STATUS "tf_tree: corrosion source build, feature set not chosen here; TFT_HAVE_SHM off")
elseif(NOT _tf_tree_static AND NOT _tf_tree_shared)
  message(STATUS "tf_tree: no library to probe yet (source build); TFT_HAVE_SHM off")
else()
  tf_tree_probe_shm("${_tf_tree_static}" TF_TREE_HAVE_SHM_STATIC)
  tf_tree_probe_shm("${_tf_tree_shared}" TF_TREE_HAVE_SHM_SHARED)
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

  # **One answer per target.** These are resolved by two separate `find_library`
  # calls and, on the `TF_TREE_PREBUILT_DIR` path, can be two different builds —
  # see the probe's comment. Each carries what its own artifact actually has.
  if(TF_TREE_HAVE_SHM_STATIC)
    set_property(TARGET tf_tree_static APPEND
                 PROPERTY INTERFACE_COMPILE_DEFINITIONS TFT_HAVE_SHM=1)
  endif()
  if(TF_TREE_HAVE_SHM_SHARED)
    set_property(TARGET tf_tree_shared APPEND
                 PROPERTY INTERFACE_COMPILE_DEFINITIONS TFT_HAVE_SHM=1)
  endif()
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
