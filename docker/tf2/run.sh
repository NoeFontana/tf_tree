#!/usr/bin/env bash
# Run a command inside the ROS 2 tf2 build environment.
#
#   docker/tf2/run.sh cargo test -p tf_tree_bench --features tf2 --release
#   docker/tf2/run.sh          # interactive shell
#
# The repository is bind-mounted at /work, so edits on the host are visible
# immediately. Cargo's target directory is redirected to `target/tf2-docker` so
# container builds (different libc, different toolchain prefix) never collide
# with host builds in `target/`.
set -euo pipefail

IMAGE="${TF_TREE_TF2_IMAGE:-tf_tree/tf2-bench}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "building $IMAGE (first run only) ..." >&2
    docker build -t "$IMAGE" "$REPO_ROOT/docker/tf2"
fi

# Only allocate a TTY when there is one, so this works from CI and from
# non-interactive tooling as well as from a terminal.
TTY_FLAGS=()
if [ -t 0 ] && [ -t 1 ]; then TTY_FLAGS=(-it); fi

# **`ROS_HOME` is set below because `-u` leaves the container with no passwd
# entry, and therefore no `$HOME`.** `rcl_logging_spdlog` falls back to
# `$HOME/.ros/log`, which resolves to `//.ros/log` and fails with EACCES — all
# seven `tf_tree_ros` ctests abort with "failed to configure logging" before
# running a line of their own code.
#
# It never fired on a developer machine, and that is exactly why it survived:
# UID 1000 *does* have an entry in this image (`/home/ubuntu`), so `just
# ros-test` passes locally. It failed on the first CI run this repository has
# had since 2026-07-23, whose runner UID has none. Reproduced in one command:
#   docker run --rm -u 4242:4242 <image> bash -lc 'echo $HOME'   ->  /
#
# `/tmp` rather than somewhere under `/work`: the bind mount is owned by the
# host user, so a container UID that does not own it cannot write there either
# — measured, `mkdir` under `/work/target` as 4242 is EACCES too. A test run's
# logs are ephemeral, so a container-local path is the right home for them.
# Match the host UID so files the container writes into the bind mount stay
# editable on the host.
exec docker run --rm "${TTY_FLAGS[@]}" \
    -v "$REPO_ROOT":/work \
    -e CARGO_TARGET_DIR=/work/target/tf2-docker \
    -e CARGO_HOME=/work/target/tf2-docker/cargo-home \
    -e ROS_HOME=/tmp/.ros \
    -u "$(id -u):$(id -g)" \
    -w /work \
    "$IMAGE" \
    bash -lc "${*:-bash}"
