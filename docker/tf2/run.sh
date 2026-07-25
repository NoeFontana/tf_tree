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

# Match the host UID so files the container writes into the bind mount stay
# editable on the host.
exec docker run --rm "${TTY_FLAGS[@]}" \
    -v "$REPO_ROOT":/work \
    -e CARGO_TARGET_DIR=/work/target/tf2-docker \
    -e CARGO_HOME=/work/target/tf2-docker/cargo-home \
    -u "$(id -u):$(id -g)" \
    -w /work \
    "$IMAGE" \
    bash -lc "${*:-bash}"
