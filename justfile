# The single task surface for tf_tree. CI mirrors these recipes 1:1.

default:
    @just --list

# Build the whole workspace.
build:
    cargo build --workspace --all-targets

# Run the full test suite (unit + integration + doctests).
test: test-rust test-doc

test-rust:
    cargo nextest run --workspace --no-tests=pass
    # **`tf_tree_c`'s `bridge` feature is default-off, so `--workspace` compiles
    # none of it.** `crates/tf_tree_c/src/bridge.rs` — nine `extern "C"` entry
    # points, the only ones that both decide and write — could be replaced with
    # literal garbage and `cargo build --workspace --all-targets`, this
    # `nextest` line above and `cargo clippy --workspace --all-targets` all
    # still passed; only `cargo fmt --check` noticed. Its 21 tests ran nowhere
    # but the two `+nightly` rows of `just c-abi-check`, so on a machine without
    # a nightly toolchain the §5 seam had no gate at all.
    #
    # This is the same shape `shm-check` exists for, one crate over: a
    # default-off feature is invisible to `--workspace`, and a file nobody
    # compiles is not a checked file.
    cargo nextest run -p tf_tree_c --features bridge

test-doc:
    cargo test --doc --workspace

# Concurrency model checking under loom (reduced buffer capacities).
loom:
    cargo xtask loom

# Undefined-behavior checking under Miri (arena + core).
#
# `miri-soft-float` is opt-in here and nowhere else: Miri cannot execute the x86
# inline `sqrt` asm libm's default `arch` feature emits. Enabling it globally
# would compile the shipped binaries and the benchmarks with soft floats too.
miri:
    cargo +nightly miri test -p tf_tree_arena -p tf_tree_core \
        --features tf_tree_core/miri-soft-float

# **The C ABI under Miri and ASan — `docs/PHASE4.md` §6.1 and §7 gate 4.**
#
# `miri` above deliberately excludes `tf_tree_c`, because the crate did not exist
# when that recipe was written. It must not stay excluded: the C ABI is the only
# place in the workspace where `unsafe` faces a caller the compiler cannot see,
# and Miri caught a real alignment UB there — in a *test* that claimed foreign
# pointers were safely rejected.
#
# `-Zmiri-disable-isolation` is needed because the fixture reads the clock.
# ASan needs `-Zbuild-std` so the standard library is instrumented too; without
# it a use-after-free inside `Box::from_raw` is invisible.
#
# The C ABI under Miri and ASan (PHASE4 §6.1, §7 gate 4).
c-abi-check:
    MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test \
        -p tf_tree_c -p tf_tree_core \
        --features tf_tree_c/test-hooks,tf_tree_core/miri-soft-float --test abi
    MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test \
        -p tf_tree_c -p tf_tree_core \
        --features tf_tree_c/test-hooks,tf_tree_core/miri-soft-float --test live
    # The publish surface. Separate because `--test` takes one target: this is
    # where a foreign *write* into the arena is checked, which is the half where
    # a mistake corrupts somebody else's transform tree rather than only
    # returning this process a bad answer.
    MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test \
        -p tf_tree_c -p tf_tree_core \
        --features tf_tree_c/test-hooks,tf_tree_core/miri-soft-float --test publish
    # The ingest-bridge seam (§5), behind the default-off `bridge` feature. It
    # is the only entry point that both *decides* and *writes*, and its outcome
    # POD hands C a fistful of `const char *` borrowed from the handle — a
    # lifetime rule no compiler on either side enforces. Miri is what checks it.
    MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test \
        -p tf_tree_c -p tf_tree_core \
        --features tf_tree_c/test-hooks,tf_tree_c/bridge,tf_tree_core/miri-soft-float \
        --test bridge
    RUSTFLAGS=-Zsanitizer=address cargo +nightly test -p tf_tree_c \
        --features test-hooks,bridge --target x86_64-unknown-linux-gnu -Zbuild-std

# **The committed C headers: drift check, then compile and run them.**
#
# Two things, and the second is what makes the first mean anything:
#
# 1. `cargo xtask headers --check` fails if `crates/tf_tree_c/include/*.h` and
#    `crates/tf_tree_c/src/` have drifted. The headers are committed on purpose
#    (`docs/decisions/0007`) so an ABI change is a diff somebody approves rather
#    than something that materialises during a build.
# 2. `tests/c/abi_smoke.c` is built and run against **gcc and clang, as C11 and
#    as C++17**, with `-Wall -Wextra -Wpedantic -Werror`. This is `docs/PHASE4.md`
#    §6.2's two-compiler matrix, and it is the only test that sees the *header*
#    rather than the crate. It is not a formality: an earlier revision of
#    `xtask headers` produced a header with an unbalanced `#endif`, and every
#    Rust test still passed.
#
# `cbindgen` is needed only for step 1's regeneration and is deliberately not a
# workspace dependency (MPL-2.0 against `deny.toml`'s allowlist). Install it with
# `cargo install cbindgen`.
c-header-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo xtask headers --check
    # **`bridge` is in this build, and the smoke test is compiled with
    # `-DTFT_HAVE_BRIDGE`.** The bridge declarations are emitted inside
    # `#if defined(TFT_HAVE_BRIDGE)`, so without both halves the nine §5 entry
    # points would be in the committed header and compiled by nothing — which is
    # exactly the state that let a function and a typedef share the name
    # `tft_bridge_stats` through a whole revision. A header nobody compiles is
    # not a checked header.
    cargo build --release -q -p tf_tree_c --features test-hooks,bridge
    inc=crates/tf_tree_c/include
    lib=target/release/libtf_tree_c.a
    src=crates/tf_tree_c/tests/c/abi_smoke.c
    out=$(mktemp -d)
    trap 'rm -rf "$out"' EXIT
    # A `.cpp` copy for the C++ rows. The obvious alternative, `-x c++`, is a
    # trap: it applies to every input that FOLLOWS it, so the 40 MB static
    # archive gets handed to the C++ front end as source. It does not fail —
    # it spins, at 100 % of a core, indefinitely.
    cp "$src" "$out/smoke.cpp"
    for cc in "gcc -std=c11" "clang -std=c11" "g++ -std=c++17" "clang++ -std=c++17"; do
        in="$src"
        case "$cc" in g++*|clang++*) in="$out/smoke.cpp";; esac
        printf '%-22s ' "$cc"
        $cc -Wall -Wextra -Wpedantic -Werror -DTFT_HAVE_BRIDGE -I "$inc" \
            -o "$out/smoke" "$in" "$lib" -lpthread -ldl -lm
        "$out/smoke"
    done

# **The C++ wrapper across §6.2's full matrix, then ASan/UBSan.**
#
# 2 compilers x 2 standards x 2 error modes = 8 builds, each of which is also
# *run*: the wrapper is header-only inline code, so "it compiles" and "it
# computes the right transform" are different claims and §6.2 wants both.
#
# Sophus is optional. Its absence is reported rather than skipped silently,
# because §4.3's stride hazard only exists where Sophus does — `sizeof(SE3d)`
# is 64 against a 56-byte payload on this host, so an array of them is NOT
# tightly packed and a packed write would corrupt every element after the
# first. Run `just cpp-deps` once to fetch it.
cpp-check:
    ./crates/tf_tree_c/tests/cpp/run.sh

# **The CMake package, proved by a downstream consumer** (§4.4).
#
# Configure, build, install, then build a separate project that reaches tf_tree
# only through `find_package(tf_tree CONFIG)` — no include path, no -lpthread,
# no -std flag. All three must arrive through the imported target. Three real
# packaging defects were invisible until that consumer existed; the script says
# which.
cmake-check:
    ./crates/tf_tree_c/tests/cmake_consumer/run.sh

# **The §7 gate-2 benchmark: the C++ wrapper against the raw C ABI.**
#
# The wrapper is inline code over an `extern "C"` call, so the gate is a tight
# 2 % — anything more means it is not inline. Also reports §7's Eigen batch and
# Sophus strided-vs-packed rows.
#
# Pinned, because an unpinned run migrates cores and swings by more than the
# gate allows.
cpp-bench:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -q -p tf_tree_c --features test-hooks
    out=$(mktemp -d); trap 'rm -rf "$out"' EXIT
    sophus=""
    [ -f target/thirdparty/Sophus/sophus/se3.hpp ] && \
        sophus="-isystem target/thirdparty/Sophus -DSOPHUS_USE_BASIC_LOGGING"
    # **Both error modes.** The gate applies to the wrapper, and
    # `-fno-exceptions` is a different wrapper: a first implementation of
    # `expected<T>` made an FFI call per success and missed the gate at 1.064x
    # while the exceptions build measured 1.002x. Measuring one mode and
    # reporting "gate 2 passes" was wrong, and this is the fix.
    for mode in "" "-fno-exceptions"; do
        g++ -O2 -std=c++17 $mode -Wall -Wextra -Werror -I crates/tf_tree_c/include \
            -isystem /usr/include/eigen3 $sophus -o "$out/bench" \
            crates/tf_tree_c/tests/cpp/bench.cpp target/release/libtf_tree_c.a \
            -lpthread -ldl -lm
        taskset -c 2 "$out/bench"
        echo
    done

# Fetch Sophus (header-only) into target/thirdparty so `cpp-check` can exercise
# §4.3. Not vendored: it is a test dependency of one recipe, it is ~10 MB, and
# putting somebody else's headers in the repo to test a stride is a poor trade.
cpp-deps:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p target/thirdparty
    if [ -d target/thirdparty/Sophus ]; then
        echo "cpp-deps: Sophus already present"
        exit 0
    fi
    git clone -q --depth 1 --branch 1.22.10 \
        https://github.com/strasdat/Sophus.git target/thirdparty/Sophus
    echo "cpp-deps: fetched Sophus 1.22.10"

# Lint everything. Pure checks; does not mutate files.
lint: py-compile
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    # The ingest-bridge seam (`docs/PHASE4.md` §5). Default-off, so the line
    # above compiles none of it — two real warnings injected into `bridge.rs`
    # left `clippy --workspace --all-targets` exiting 0. See `test-rust`.
    cargo clippy -p tf_tree_c --features bridge --all-targets -- -D warnings
    # `test-hooks` has the same hole: `tests/publish.rs` and `examples/abi_cost.rs`
    # are `#![cfg(feature = "test-hooks")]`, so the workspace pass compiles them
    # to nothing and `just c-abi-check` only ever runs `cargo test` over them.
    # One real warning had been sitting in `publish.rs` unseen.
    cargo clippy -p tf_tree_c --features test-hooks --all-targets -- -D warnings

# **`tf_tree_py` is excluded from the workspace, so nothing else builds it.**
#
# That gap shipped a real bug: `PyPublisher` held a
# `transmute::<EdgeWriter, Publisher>` which compiled only while the two types
# happened to be the same size, and which silently discarded the claim lease and
# the fork guard. It went unnoticed across six PRs because `just test` and
# `just lint` never compiled the crate at all.
#
# A compile is cheap and needs only an interpreter, so `lint` depends on it. It
# is skipped with a loud message rather than failing when no venv exists — a
# clean checkout should not be blocked on `just py-setup` — but on any machine
# that has ever run the Python suite, it is a real gate.
py-compile:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -x .venv/bin/python ]; then
        echo "py-compile: SKIPPED — no .venv. Run \`just py-setup\` to gate the bindings." >&2
        exit 0
    fi
    PYO3_PYTHON=$PWD/.venv/bin/python cargo clippy \
        --manifest-path crates/tf_tree_py/Cargo.toml --all-targets -- -D warnings

# Format and auto-fix safe lint issues.
fmt:
    cargo fmt --all
    cargo clippy --workspace --all-targets --fix --allow-dirty -- -D warnings

# cargo-deny: advisories, licenses, bans, sources.
audit:
    cargo deny check

# Run the benchmark suite and the go/no-go gate.
bench:
    cargo xtask bench-gate

# **`docs/PHASE5.md` §9's benchmark artifact.** One command, one `report/`
# directory: `results.json` (stable schema, CI-diffable), `index.html`, and the
# provenance header §9.3 requires.
#
# `--release` is not optional and not a speed convenience: the tool refuses to
# call any timing row a claim in a debug build, so a debug run produces a report
# with every timing row marked UNAVAILABLE for a reason that is about the build
# rather than about the engine.
#
# On a host that cannot measure a row fairly the row is printed UNAVAILABLE with
# the reason and the command that produces it elsewhere — never estimated, never
# dropped. `TF_TREE_BENCH_FORCE=1` downgrades those rows to `indicative` (labelled
# in both outputs as not a claim) rather than upgrading them to measured.
bench-report *ARGS:
    cargo run --release -p tf_tree_bench --bin bench_report -- {{ARGS}}

# **The same report, built with the frozen backend compiled in.**
#
# `bench-report` above does NOT pass `--features shm`, and that is not an
# oversight — `shm` is off by default so the single-process build never grows a
# syscall dependency it does not use, and it is Linux-only. The consequence is
# concrete and shows up in the artifact: `tf_tree::Tree::open_frozen` is
# `#[cfg(all(feature = "shm", target_os = "linux"))]`, so in the default build
# the two `.tft` rows are UNAVAILABLE because *the function is not compiled in*.
# The report says exactly that.
#
# This recipe exists so that reason is falsifiable rather than decorative: it is
# the command those two rows name as the way to get past it, and a `reproduce:`
# line naming a recipe that does not exist is checked by
# `report::tests::every_command_the_report_names_is_a_command_that_exists`.
#
# The rows still need a representative `.tft` (§12 gate 2 is written about a
# 233 MB index) and 16 physical cores, neither of which a cargo feature can
# supply — so on this host they stay UNAVAILABLE, with the *second* reason
# rather than the first. That is the point: each blocker is stated as it is
# reached, and none of them is a claim about which phase has landed.
bench-report-shm *ARGS:
    cargo run --release -p tf_tree_bench --features shm --bin bench_report -- {{ARGS}}

# **`docs/PHASE5.md` §10's "benchmark artifact as a regression gate".**
#
# Regenerates the report and compares it against
# `crates/tf_tree_bench/baseline/results.json`, exiting non-zero if this build
# withdrew a claim, dropped a row, changed the arena layout, or moved a
# directional number past the slack the baseline records.
#
# **What it does NOT do is compare the host.** CPU model, core count, kernel,
# governor, load and every `reason` string are ignored, because they differ on
# every machine and a gate that fails for the CPU model is a gate people learn
# to ignore. `src/baseline.rs` carries the full split.
#
# On this host exactly one row is a claim — the LerpSlerp differential, which is
# host-independent by construction — so this gate holds one number today. That
# is not a placeholder: `Report::validate` refuses any row that prints numbers
# without giving at least one of them a direction, so a row that starts being
# measurable arrives already gated or not at all.
#
# `--out target/bench-report` and not `report/`: this is a check, and it should
# not clobber a report somebody generated to look at.
bench-check:
    cargo run --release -p tf_tree_bench --bin bench_report -- \
        --out target/bench-report \
        --check-baseline crates/tf_tree_bench/baseline/results.json

# Regenerate the committed baseline. **Run this deliberately, and put the diff
# in the same commit as the change that causes it** — the diff is the record of
# what moved and it is the only place a reviewer sees it.
#
# `index.html` is not committed: it is a rendering of `results.json` and a second
# copy that can disagree with the first.
bench-baseline-update:
    cargo run --release -p tf_tree_bench --bin bench_report -- --out target/bench-report
    cp target/bench-report/results.json crates/tf_tree_bench/baseline/results.json

# `tf_tree_tf2_sys` is deliberately excluded from the workspace (it only builds
# where ROS 2 is installed), which also excludes it from `cargo fmt --all`,
# `cargo clippy --workspace` and `cargo nextest run --workspace`. It is the one
# crate in the repo carrying `unsafe` with no lint coverage, and its unit tests —
# the quaternion-convention guard and the Send/Sync justification among them —
# run nowhere else. This recipe is where they run. Run it whenever anything under
# `crates/tf_tree_tf2_sys/` or behind `tf_tree_bench`'s `tf2` feature changes.
#
# `--manifest-path` is how the excluded crate is addressed: from the workspace
# root, `-p tf_tree_tf2_sys` does not resolve.
#
# The `tf_tree_bench` half is what puts the feature-gated code — the benches, the
# `tf2_scaling` binary, the tf2 modules — under clippy at all; without
# `--features tf2` the host's `just lint` compiles none of it. Its `--lib` run
# finds no unit tests today (`--no-tests=pass`), and is there so that one added
# behind the feature actually runs; the tf2 *integration* tests keep their own
# recipes, `tf2-differential` and `tf2-replay`.
#
# The `tf_tree_c --features bridge` clippy row is **deliberately the same command
# `just lint` runs**, not a copy-paste slip. `ros/tf_tree_ros` links a
# `libtf_tree_c.a` built by *this image's* rustup toolchain, which is installed
# independently of the host's and is pinned only by the Dockerfile's
# `RUST_TOOLCHAIN` argument. This is the recipe to run before `just ros-build`,
# and it is where a container-side toolchain drift in the one crate the ROS
# package links shows up.
#
# fmt + clippy + unit tests for the tf2 bridge, in the container. `lint` and `test` cannot see it.
tf2-check:
    ./docker/tf2/run.sh 'set -euo pipefail; \
        cargo fmt --manifest-path crates/tf_tree_tf2_sys/Cargo.toml -- --check; \
        cargo clippy --manifest-path crates/tf_tree_tf2_sys/Cargo.toml --all-targets -- -D warnings; \
        cargo nextest run --manifest-path crates/tf_tree_tf2_sys/Cargo.toml --release; \
        cargo clippy -p tf_tree_bench --features tf2 --all-targets -- -D warnings; \
        cargo nextest run -p tf_tree_bench --features tf2 --release --lib --no-tests=pass; \
        cargo clippy -p tf_tree_c --features bridge --all-targets -- -D warnings'

# **The ROS 2 ingest bridge (`docs/PHASE4.md` §5), built in the container.**
#
# `ros/tf_tree_ros` is an `ament_cmake` package, not a cargo crate: it needs
# `rclcpp`, which exists only in `docker/tf2`. It therefore inherits
# `tf_tree_tf2_sys`' problem — no `cargo fmt`, no `clippy`, no `nextest` — and
# these two recipes are the whole of its gate. Run them after touching anything
# under `ros/`.
#
# `ros/build.sh` says what the three steps are and which of them is easy to get
# wrong; the short version is that the staticlib must carry `--features bridge`
# and colcon must be told every output directory or it litters the repo root.
#
# Build ros/tf_tree_ros (PHASE4 §5) in the container. Nothing on the host can.
ros-build:
    ./docker/tf2/run.sh './ros/build.sh'

# The same build, then its `ctest`s — §6.3's QoS regression among them, which
# needs a real DDS and two participants and so can exist nowhere else.
#
# Build ros/tf_tree_ros and run its ctests. The only gate this package has.
ros-test:
    ./docker/tf2/run.sh './ros/build.sh --test'

# The tf2::BufferCore differential — the migration-credibility test.
#
# Runs in a container (ROS 2 Lyrical) so no ROS install is needed on the host.
# First run builds the image; afterwards it is cached.
#
# Everything below is container-only, as is `tf2-check` above — run that one
# after touching the bridge, since no host-side recipe lints or tests it.
tf2-differential:
    ./docker/tf2/run.sh 'cargo test -p tf_tree_bench --features tf2 --release --test differential -- --nocapture'

# The same differential, but over a real recorded /tf stream (see
# testdata/tfstream/ATTRIBUTION.md for provenance and licensing).
tf2-replay:
    ./docker/tf2/run.sh 'cargo test -p tf_tree_bench --features tf2 --release --test replay -- --nocapture'

# Head-to-head performance against tf2. Indicative unless run on pinned cores —
# see docs/benchmarks/tf2.md for the caveats and the pinned-hardware runbook.
tf2-bench:
    ./docker/tf2/run.sh 'cargo bench -p tf_tree_bench --features tf2 --bench tf2_compare'

# Concurrent read scaling at 1/2/4/8 threads — tf_tree's lock-free readers vs
# tf2's per-lookup mutex. Reports p50/p99/p99.9, not means.
#
# RUN THIS ON AN IDLE MACHINE. Competing load makes the 8-thread rows worthless.
tf2-scaling:
    ./docker/tf2/run.sh 'cargo run -p tf_tree_bench --features tf2 --release --bin tf2_scaling'

# Memory footprint and computation-per-lookup vs tf2 (docs/benchmarks/tf2.md).
#
# Not timing-based, so unlike `tf2-bench` and `tf2-scaling` this does NOT need an
# idle machine: cachegrind and memcheck simulate, so every number here is exact
# and reproducible under load. It is slow for the same reason (~50x).
#
# Each mode runs in its own process on purpose — building both engines in one
# would let the first's freed chunks satisfy the second's requests.
footprint:
    ./docker/tf2/run.sh 'set -e; cargo build --release -q -p tf_tree_bench --features tf2 --bin footprint; \
        B=./target/tf2-docker/release/footprint; \
        echo "=== memory: identical topology + 10 s of history ==="; \
        $B mem-tf_tree; $B mem-tf2; \
        echo; echo "=== computation: cachegrind, N=0 baseline subtracted ==="; \
        for m in lookup-tf_tree lookup-tf_tree-sclerp lookup-tf2; do \
          for n in 0 100000; do \
            valgrind --tool=cachegrind --cache-sim=yes --branch-sim=yes \
              --cachegrind-out-file=/dev/null $B $m $n 2>&1 \
              | grep -E "I refs|D1  misses|LLd misses|Mispredicts" | tr -s " " | sed "s|^|[$m n=$n] |"; \
          done; \
        done; \
        echo; echo "=== allocations: memcheck, N=0 baseline subtracted ==="; \
        for m in lookup-tf_tree lookup-tf2 push-tf_tree push-tf2; do \
          for n in 0 10000; do \
            valgrind --tool=memcheck $B $m $n 2>&1 \
              | grep "total heap usage" | sed "s|^|[$m n=$n] |"; \
          done; \
        done'

# Line-level profile of the lookup hot path (docs/benchmarks/tf2.md).
#
# Uses the `profiling` profile — release codegen with debuginfo kept — because
# `[profile.release]` strips it and every tool then falls back to function-level
# attribution. `fold_at` inlines the whole sampling chain, so a function-level
# answer is "it is all in fold_at", which is true and tells you nothing.
#
# Simulated, so no idle machine is needed and the counts are exact.
profile-lookup n="200000":
    ./docker/tf2/run.sh 'set -e; \
        cargo build --profile profiling -q -p tf_tree_bench --features tf2 --bin footprint; \
        B=./target/tf2-docker/profiling/footprint; \
        valgrind --tool=cachegrind --branch-sim=yes --cache-sim=yes \
            --cachegrind-out-file=/tmp/cg.out $B lookup-tf_tree {{n}} >/dev/null 2>&1; \
        cg_annotate --show=Ir,Bcm,D1mr --sort=Ir --auto=yes /tmp/cg.out'

# ThreadSanitizer over the concurrent read path (PHASE3 §7.3).
#
# Complements `just loom`, which model-checks the protocols exhaustively but
# over `loom::sync` substitutes with a bounded interleaving budget. TSan runs
# real threads against the real generated code, so it sees races the model
# cannot — one introduced by the facade rather than the protocol.
#
# This is also what makes `tf_tree_py`'s `gil_used = false` honest: PyO3 0.29
# defaults that flag to false, so no test of the attribute can be non-vacuous
# (PHASE3 §1.2, corrected). The declaration rests on the Rust underneath being
# race-free, which is what this checks.
#
# `-Zbuild-std` because std must be instrumented too — an uninstrumented std
# reports false positives on its own internals and misses real races through
# them.
tsan:
    RUSTFLAGS="-Zsanitizer=thread" \
    cargo +nightly test -Zbuild-std --target x86_64-unknown-linux-gnu \
        -p tf_tree --features shm --test tsan --release

# --- Phase 2: shared memory (Linux only) -------------------------------------
#
# `shm` is off by default so the single-process build never grows a syscall
# dependency it does not use. These recipes need no container: shared memory is
# a kernel feature, not a ROS one.

# Multi-process gate: a second process maps the same arena and must answer
# bit-identically. Builds `shm_child` first — the test spawns it.
shm-test:
    cargo build --features shm -p tf_tree_bench --bin shm_child
    cargo nextest run -p tf_tree_bench --features shm --test multiprocess

# N reader processes on one shared arena, plus the memory that sharing saves.
# RUN THIS ON AN IDLE MACHINE — the 8-process row oversubscribes 4 cores 2:1.
shm-scaling:
    cargo build --release --features shm -p tf_tree_bench --bins
    ./target/release/shm_scaling

# Multi-process NODE evaluation: N consumer processes at a fixed rate, with a
# live publisher. Answers the deployment question ("what does each node
# experience, and what does it cost?") rather than shm-scaling's roofline
# question ("how many lookups can N processes extract in total?").
#
# REFUSES TO RUN on a busy machine and names what is running — latency here is
# largely a measurement of the scheduler, so a number taken against somebody
# else's workload describes that workload. Override with TF_TREE_BENCH_FORCE=1
# only if you are certain the load is irrelevant.
mp-bench:
    cargo build --release --features shm -p tf_tree_bench --bins
    taskset -c 0-7 ./target/release/mp_bench tf_tree

# The same evaluation against tf2, in the container. The tf2 column is a FLOOR:
# each consumer holds a private BufferCore built from the identical stream, so it
# shows the duplication that having no shared arena forces, but no transport.
# A deployed tf2 consumer reaches the tree only over DDS and pays more.
mp-bench-tf2:
    ./docker/tf2/run.sh 'set -e; cargo build --release --features "shm tf2" -p tf_tree_bench --bins; \
        ./target/tf2-docker/release/mp_bench tf_tree; \
        echo; ./target/tf2-docker/release/mp_bench tf2'

# fmt + clippy + tests for everything behind the `shm` feature, which plain
# `just lint` and `just test` do not compile.
#
# `tf_tree + tf_tree_ipc` only exist together under `--features shm`
# (`docs/decisions/0005`), so `--workspace` never sees the seam at all.
shm-check:
    cargo clippy -p tf_tree_arena --features shm --all-targets -- -D warnings
    cargo clippy -p tf_tree --features shm --all-targets -- -D warnings
    cargo clippy -p tf_tree_ipc --all-targets -- -D warnings
    cargo clippy -p tf_tree_bench --features shm --all-targets -- -D warnings
    cargo clippy -p tf_tree_cli --features shm --all-targets -- -D warnings
    cargo build --features shm -p tf_tree_bench --bin shm_child
    cargo build --features shm -p tf_tree_bench --bin fork_child
    cargo nextest run -p tf_tree_bench --features shm --test multiprocess
    # Fork poisoning (`docs/decisions/0005` step 9). Separate from
    # `shm-rendezvous` because it needs no second executable and no scratch
    # rendezvous beyond its own: the second process is a `fork` of the first.
    cargo nextest run -p tf_tree_bench --features shm --test fork
    # §7.1 page population. `nextest` runs each test in its own process, which
    # this needs: the measurements are RSS and minor-fault deltas, and threads
    # sharing a process would read each other's.
    cargo nextest run -p tf_tree_bench --features shm --test population
    # The CLI against a live arena, and `participants` against no arena at all.
    # This is the milestone's acceptance test: the shipped binary, through clap,
    # joining somebody else's tree.
    cargo nextest run -p tf_tree_cli --features shm --test attach
    # §7's `--web` view under the same feature. `--workspace` runs `tests/web.rs`
    # without `shm`, and that is a different binary: `cmd_top_web` calls the
    # `merge` closure that only exists under `shm`, so the build an operator
    # actually attaches with was compiled by clippy here and executed nowhere.
    cargo nextest run -p tf_tree_cli --features shm --test web
    # **The frozen `.tft` arena (`docs/PHASE5.md` §2), which needs a real
    # mapping and therefore `--features shm`.** Without these two lines the
    # branch that introduced it had its centrepiece — §2.1's bit-for-bit proof
    # that a frozen file is read by the identical `Plan::at` code as a live
    # arena — running in **no gate at all**: `--workspace` builds without `shm`,
    # and this recipe named every other shm target but not these.
    #
    # That is the same gap that let a real bug live in `tf_tree_py` across six
    # PRs, and it is why a new shm-only target belongs here in the same commit
    # that adds it.
    cargo nextest run -p tf_tree_arena --features shm
    cargo nextest run -p tf_tree --features shm --test frozen
    # **`docs/PHASE5.md` §3 into §2's container.** `tests/frozen_bag.rs` carries
    # `required-features = ["shm"]`, so `cargo nextest run --workspace` — which
    # builds without features — skips the target entirely. The rest of
    # `tf_tree_ingest`'s suite does run there; this is the half that cannot.
    cargo clippy -p tf_tree_ingest --features shm --all-targets -- -D warnings
    cargo nextest run -p tf_tree_ingest --features shm
    cargo nextest run -p tf_tree_ipc

# The zero-config rendezvous end to end: a foreign process calls
# `tf_tree::open()`, joins a served arena, and reads the same transform.
shm-rendezvous:
    # `test-hooks` adds one injection point inside `Tree::claim`, between the
    # arena CAS and the lease `SETLK`. The window is a single syscall wide, so
    # `the_acquire_window_backs_out` cannot place a reaper inside it by racing.
    # The hook is inert when unset, so the other tests run as they always did.
    cargo nextest run -p tf_tree --features shm,test-hooks --test rendezvous

# Interactive shell in the ROS 2 / tf2 build environment.
tf2-shell:
    ./docker/tf2/run.sh

# Remove build artifacts.
clean:
    cargo clean

# Print toolchain versions.
versions:
    @echo "rustc:  $(rustc --version)"
    @echo "cargo:  $(cargo --version)"
    @echo "just:   $(just --version)"

# Pure-C++ tf2 read scaling: no Rust, no FFI. The control that proves the
# benchmark's tf2 numbers are not an artifact of our binding.
tf2-native-control:
    ./docker/tf2/run.sh 'bash docker/tf2/native_scaling.sh'

# ---------------------------------------------------------------------------
# Python bindings (docs/PHASE3.md). `tf_tree_py` is excluded from the workspace
# because it links libpython, so none of this is reachable from `just test`.
#
# Interpreters come from uv rather than the host, so the floors in PHASE3 §10.1
# are what actually gets used. 3.14 is the GIL build; 3.14t is free-threaded,
# and §7.3 requires the suite to pass on both.
# ---------------------------------------------------------------------------

# Create both venvs and install the toolchain.
py-setup:
    uv python install 3.14 3.14t
    uv venv --python 3.14 .venv
    VIRTUAL_ENV=.venv uv pip install -q maturin numpy pytest ruff pyright
    uv venv --python 3.14t .venv-t
    VIRTUAL_ENV=.venv-t uv pip install -q maturin numpy pytest

# Build the extension into the GIL venv and run the suite.
py-test:
    VIRTUAL_ENV=.venv .venv/bin/maturin develop --uv -q
    .venv/bin/python -m pytest tests/python -q

# The same on the free-threaded interpreter — §7.3's requirement, and the only
# place the concurrency claims are actually exercised.
py-test-freethreaded:
    VIRTUAL_ENV=.venv-t PYO3_PYTHON=$PWD/.venv-t/bin/python .venv-t/bin/maturin develop --uv -q
    .venv-t/bin/python -m pytest tests/python -q

# fmt + lint for both languages of the binding.
py-lint:
    cargo fmt --manifest-path crates/tf_tree_py/Cargo.toml -- --check
    PYO3_PYTHON=$PWD/.venv/bin/python cargo clippy --manifest-path crates/tf_tree_py/Cargo.toml --all-targets -- -D warnings
    .venv/bin/ruff check python tests/python crates/tf_tree_bench/python
    .venv/bin/ruff format --check python tests/python crates/tf_tree_bench/python
    # `--strict` over the package and its stubs (PHASE3 §9). Not over
    # tests/: numpy's own stubs are partially typed, so strict there reports
    # ~120 errors that are numpy's and not ours, and a gate nobody can keep
    # green is a gate nobody runs.
    .venv/bin/pyright python

# Build a release wheel.
py-wheel:
    VIRTUAL_ENV=.venv .venv/bin/maturin build --release

# N Python consumer nodes on one shared arena, against N private `tf2_ros`
# buffers (PHASE2 §12.4, PHASE3 §12.1).
#
# The single-process row (`py-vs-tf2`) is a latency comparison; this is the
# deployment comparison, and it is where the shared arena earns its keep: a
# Python tf2 node materialises the whole history privately, and every node pays
# for it again.
#
# RUN THIS ON AN IDLE MACHINE.
py-mp-bench:
    just py-wheel
    ./docker/tf2/run.sh 'set -e; \
        rm -rf target/pywheel && mkdir -p target/pywheel; \
        python3 -c "import zipfile,glob; zipfile.ZipFile(glob.glob(\"crates/tf_tree_py/target/wheels/tf_tree-*-cp314-*.whl\")[0]).extractall(\"target/pywheel\")"; \
        PYTHONPATH=target/pywheel:$PYTHONPATH python3 crates/tf_tree_bench/python/mp_compare.py'

# tf_tree's Python API against tf2_ros's, in the ROS container (PHASE3 §12.1).
#
# The wheel is built on the host and installed in the container: both are
# CPython 3.14, so the cp314 ABI matches. tf2 is fed its BufferCore directly —
# no DDS, no TransformListener — which is the most generous in-process
# comparison available, not the least.
py-vs-tf2:
    just py-wheel
    # The container has no pip, and does not need one: a wheel is a zip, and
    # numpy is already present (2.3.5). Unpacking onto PYTHONPATH also keeps
    # the container's system site-packages untouched.
    ./docker/tf2/run.sh 'set -e; \
        rm -rf target/pywheel && mkdir -p target/pywheel; \
        python3 -c "import zipfile,glob; zipfile.ZipFile(glob.glob(\"crates/tf_tree_py/target/wheels/tf_tree-*-cp314-*.whl\")[0]).extractall(\"target/pywheel\")"; \
        PYTHONPATH=target/pywheel:$PYTHONPATH python3 crates/tf_tree_bench/python/tf2_ros_compare.py'
