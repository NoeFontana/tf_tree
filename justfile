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

# Lint everything. Pure checks; does not mutate files.
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings

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
# fmt + clippy + unit tests for the tf2 bridge, in the container. `lint` and `test` cannot see it.
tf2-check:
    ./docker/tf2/run.sh 'set -euo pipefail; \
        cargo fmt --manifest-path crates/tf_tree_tf2_sys/Cargo.toml -- --check; \
        cargo clippy --manifest-path crates/tf_tree_tf2_sys/Cargo.toml --all-targets -- -D warnings; \
        cargo nextest run --manifest-path crates/tf_tree_tf2_sys/Cargo.toml --release; \
        cargo clippy -p tf_tree_bench --features tf2 --all-targets -- -D warnings; \
        cargo nextest run -p tf_tree_bench --features tf2 --release --lib --no-tests=pass'

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
