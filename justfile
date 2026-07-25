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

# The tf2::BufferCore differential — the migration-credibility test.
#
# Runs in a container (ROS 2 Lyrical) so no ROS install is needed on the host.
# First run builds the image; afterwards it is cached.
tf2-differential:
    ./docker/tf2/run.sh 'cargo test -p tf_tree_bench --features tf2 --release --test differential -- --nocapture'

# The same differential, but over a real recorded /tf stream (see
# testdata/tfstream/ATTRIBUTION.md for provenance and licensing).
tf2-replay:
    ./docker/tf2/run.sh 'cargo test -p tf_tree_bench --features tf2 --release --test replay -- --nocapture'

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
