# The single task surface for rust-python-template. CI mirrors these recipes 1:1.

default:
    @just --list

# Sync the Python venv and build the FFI extension in release mode.
bootstrap:
    uv sync --all-extras
    uv run maturin develop --release

# Rebuild the FFI extension in debug mode for fast iteration.
develop:
    uv run maturin develop

# Build a release wheel into target/wheels/.
build:
    uv run maturin build --release

# Run the full Rust + Python test suite.
test: test-rust test-py

test-rust:
    cargo nextest run --workspace --exclude rust-python-template-ffi

test-py:
    uv run pytest

# Lint everything (Rust + Python). Pure checks; does not mutate files.
lint: lint-rust lint-py

lint-rust:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings

lint-py:
    uv run ruff check .
    uv run ruff format --check .
    uv run pyright

# Format everything (Rust + Python) and auto-fix safe lint issues.
fmt:
    cargo fmt --all
    uv run ruff format .
    uv run ruff check --fix .

# Run cargo-deny (advisories, licenses, bans, sources).
audit:
    cargo deny check

# Remove build artifacts and the venv.
clean:
    cargo clean
    rm -rf .venv target/wheels
    find python -name "_core*.so" -delete -o -name "_core*.pyd" -delete

# Print versions of all required toolchains.
versions:
    @echo "rustc:  $(rustc --version)"
    @echo "cargo:  $(cargo --version)"
    @echo "uv:     $(uv --version)"
    @echo "just:   $(just --version)"
    @echo "python: $(uv run python --version)"
