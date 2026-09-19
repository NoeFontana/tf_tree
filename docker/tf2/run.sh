#!/usr/bin/env bash
# Run a command inside the ROS 2 tf2 build environment.
#
#   docker/tf2/run.sh cargo test -p tf_tree_bench --features tf2 --release
#   docker/tf2/run.sh          # interactive shell
#
# The repo is bind-mounted at /work; cargo's target dir is `target/tf2-docker` so
# container builds never collide with host builds.
set -euo pipefail

IMAGE="${TF_TREE_TF2_IMAGE:-tf_tree/tf2-bench}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "building $IMAGE (first run only) ..." >&2
    docker build -t "$IMAGE" "$REPO_ROOT/docker/tf2"
fi

# TTY only when there is one (CI).
TTY_FLAGS=()
if [ -t 0 ] && [ -t 1 ]; then TTY_FLAGS=(-it); fi

# `ROS_HOME` is set because `-u` with a UID lacking a passwd entry leaves no
# `$HOME`, and `rcl_logging_spdlog` then fails with EACCES at `//.ros/log`. It is
# under `/tmp` because the bind mount is not writable by such a UID. `-u` matches
# the host so written files stay editable.
exec docker run --rm "${TTY_FLAGS[@]}" \
    -v "$REPO_ROOT":/work \
    -e CARGO_TARGET_DIR=/work/target/tf2-docker \
    -e CARGO_HOME=/work/target/tf2-docker/cargo-home \
    -e ROS_HOME=/tmp/.ros \
    -u "$(id -u):$(id -g)" \
    -w /work \
    "$IMAGE" \
    bash -lc "${*:-bash}"
