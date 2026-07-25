# Build environment for the tf2 differential and comparison benchmarks.
#
# ROS 2 Lyrical Luth (May 2026, LTS to May 2031) on Ubuntu 26.04 "Resolute" —
# the current LTS-on-LTS pair. `ros-base` rather than a desktop image because
# `tf2::BufferCore` links against `-ltf2` alone: no rclcpp, no DDS, no ROS graph.
# That keeps the image small and, more importantly, keeps middleware out of the
# measurement so the comparison is library-vs-library.
FROM ros:lyrical-ros-base

# Must match `rust-toolchain.toml` at the repo root, or rustup tries to sync the
# pinned channel at run time — which fails, because the toolchain directory is
# baked into the image and the container runs as an unprivileged host UID.
ARG RUST_TOOLCHAIN=stable

# `valgrind` is for the instructions-per-lookup comparison (`just footprint`).
# cachegrind *simulates*, so its instruction counts are exact and reproducible on
# any host — including one where `perf_event_paranoid` forbids hardware counters,
# which is the case on the machine this was developed on.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        curl \
        ca-certificates \
        pkg-config \
        valgrind \
    && rm -rf /var/lib/apt/lists/*

# Pin the toolchain to the workspace's rust-version so a container run and a host
# run compile the same code.
#
# Installed system-wide, not into /root: `run.sh` runs the container as the host
# UID so bind-mounted files stay host-editable, and that user cannot read /root.
# RUSTUP_HOME must stay pointing here even when the caller overrides CARGO_HOME
# (which it does, to keep the registry cache in the bind mount) — the `cargo`
# binary is a rustup proxy and resolves its toolchain through RUSTUP_HOME.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/bin:${PATH}
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --default-toolchain ${RUST_TOOLCHAIN} --profile minimal \
                    --component rustfmt --component clippy \
    && chmod -R a+rwX ${RUSTUP_HOME} ${CARGO_HOME}

# `cargo-nextest` for the same test runner the rest of the project uses.
RUN curl -LsSf https://get.nexte.st/latest/linux \
      | tar zxf - -C /usr/local/bin \
    && chmod a+rx /usr/local/bin/cargo-nextest

# Source ROS for every shell so `ROS_DISTRO` is set and `build.rs` finds the
# install without needing TF_TREE_ROS_PREFIX.
RUN echo '. /opt/ros/lyrical/setup.bash' >> /etc/bash.bashrc

WORKDIR /work
SHELL ["/bin/bash", "-lc"]
