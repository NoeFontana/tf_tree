# The single task surface for tf_tree. CI mirrors these recipes 1:1.

default:
    @just --list

# Build the whole workspace.
build:
    cargo build --workspace --all-targets

# Run the full test suite (unit + integration + doctests).
test: test-rust test-doc ingest-check

test-rust:
    cargo nextest run --workspace --no-tests=pass
    # `bridge` is default-off, so `--workspace` compiles none of `tf_tree_c/src/bridge.rs`; this line is its gate.
    cargo nextest run -p tf_tree_c --features bridge
    cargo nextest run -p tf_tree_core --features crash-points

test-doc:
    cargo test --doc --workspace

# **The `compile_fail` error-code pins, which stable rustdoc does not check.**
# `OwnedWriter` and `Publisher` must not be `Sync` (PROJECT §5 D7); only nightly checks the pinned code. CI runs this on the nightly job.
test-doc-error-codes:
    cargo +nightly test --doc -p tf_tree -p tf_tree_core

# **`tf_tree` with `unstable` OFF: the configuration a published consumer gets.**
# `--workspace` unifies `unstable` on, so only `-p tf_tree` sees the stable tier (API.md §2.6).
stable-tier-check:
    #!/usr/bin/env bash
    set -euo pipefail
    echo "==> the library, default features"
    cargo clippy -p tf_tree --lib -- -D warnings
    echo "==> the library, default features + shm"
    cargo clippy -p tf_tree --lib --features shm -- -D warnings
    echo "==> the library, no default features"
    cargo clippy -p tf_tree --lib --no-default-features -- -D warnings
    echo "==> the stable tier's own documentation"
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p tf_tree
    echo "==> the recorded consumers of the unstable tier are the actual ones"
    want="tf_tree_bench tf_tree_c tf_tree_cli tf_tree_py"
    want_dev="tf_tree_bridge"
    scan() {
        awk -v sect="$1" '
            /^\[/ { s = $0 }
            /^tf_tree = .*"unstable"/ { if (s == sect) print FILENAME }
        ' crates/*/Cargo.toml \
            | sed 's|^crates/||; s|/Cargo.toml$||' | sort -u | tr '\n' ' ' | sed 's/ *$//'
    }
    got=$(scan '[dependencies]')
    got_dev=$(scan '[dev-dependencies]')
    rc=0
    if [ "$got" != "$want" ]; then
        echo "[dependencies] on tf_tree/unstable: $got"
        echo "the documents say:                  $want"
        rc=1
    fi
    if [ "$got_dev" != "$want_dev" ]; then
        echo "[dev-dependencies] on tf_tree/unstable: $got_dev"
        echo "the documents say:                      $want_dev"
        rc=1
    fi
    row=$(grep -m1 '^| 4 |' docs/API.md)
    n=$(grep -n '^unstable = \[\]' crates/tf_tree/Cargo.toml | cut -d: -f1)
    blk=$(sed -n "1,$((n - 1))p" crates/tf_tree/Cargo.toml | tac | awk "/^#/ {print; next} {exit}")
    for c in $want; do
        case "$row" in
            *"\`$c\`"*) ;;
            *) echo "docs/API.md §6 row 4 does not name $c"; rc=1 ;;
        esac
        case "$blk" in
            *"\`$c\`"*) ;;
            *) echo "crates/tf_tree/Cargo.toml's 'unstable' comment does not name $c"; rc=1 ;;
        esac
    done
    [ "$rc" = 0 ] || exit 1
    echo "==> the downstream suites, run under -p so nothing unifies for them"
    cargo nextest run -p tf_tree_ingest -p tf_tree_bridge
    echo "==> the tier still lists the tests it is supposed to"
    have=$(cargo nextest list -p tf_tree 2>/dev/null | wc -l)
    [ "$have" -ge 70 ] || { echo "the stable tier lists $have tests, was 70 at 0.0.1"; exit 1; }
    have=$(cargo nextest list -p tf_tree --features unstable 2>/dev/null | wc -l)
    [ "$have" -ge 77 ] || { echo "the unstable tier lists $have tests, was 77 at 0.0.1"; exit 1; }

# Concurrency model checking under loom (reduced buffer capacities).
loom:
    cargo xtask loom

    # `miri-soft-float` is opt-in: Miri cannot execute libm's x86 `sqrt` asm.
    # `tf_tree` has its own line for `OwnedWriter`'s lifetime extension (0017); `--lib --test owned_writer` only. `MIRIFLAGS` is appended to.

# Miri over the arena, the core, and the facade's one lifetime extension.
miri:
    cargo +nightly miri test -p tf_tree_arena -p tf_tree_core \
        --features tf_tree_core/miri-soft-float
    MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-disable-isolation" cargo +nightly miri test -p tf_tree \
        --features tf_tree_core/miri-soft-float --lib --test owned_writer

# Every file carrying `unsafe` has a row in `scripts/unsafe-budget.txt` (0048).
# Compiler-driven census, slow when cold, hence last in `lint`; `--self-test` covers the comparison.
unsafe-budget:
    bash scripts/unsafe-budget.sh --self-test
    bash scripts/unsafe-budget.sh

# Every runnable artifact is executed by a recipe or registered as a probe in `docs/benchmarks/EVIDENCE.md`.
# It matches execution shapes and register ROWS, never bare names, and does not check a probe's number.
evidence-audit:
    ./scripts/evidence-audit.sh

# **The 2027 escape hatch, checked today** (#180): `cargo check` of `tf_tree_py` with `pure-hash` for macOS and Windows.
# Check, not build (linking needs the Apple SDK); the only thing that compiles `pure-hash`.
py-cross-check:
    #!/usr/bin/env bash
    set -euo pipefail
    for t in x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-msvc; do
        rustup target list --installed | grep -qx "$t" \
            || rustup target add "$t"
    done
    for t in x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-msvc; do
        echo "==> cargo check --target $t (pure-hash)"
        cargo check --manifest-path crates/tf_tree_py/Cargo.toml --target "$t" \
            --features pure-hash,pyo3/extension-module,pyo3/abi3-py39
    done
    echo "py-cross-check: the wheel's Rust half cross-compiles to macOS and Windows"

# **No tracked file is build output**, matched by signature rather than path (see the script).
no-build-output:
    ./scripts/no-build-output.sh

# **No tracked file carries an unresolved merge-conflict marker.** `=======` is deliberately not matched (setext underline).
no-conflict-markers:
    ./scripts/no-conflict-markers.sh

# **What the diagnostic counters cost a guard: `docs/decisions/0022` question 1**, {release, embedder} x {counters on, off}.
# Only a per-call guard on a WRITABLE arena pays the flush; both arenas here are writable.
guard-cost:
    #!/usr/bin/env bash
    set -euo pipefail
    for prof in release embedder; do
      for feat in "--features shm" "--no-default-features --features shm"; do
        case "$feat" in *no-default*) c=off ;; *) c=on ;; esac
        cargo build --profile "$prof" -q $feat -p tf_tree_bench --bin arena_backing
        dir=$([ "$prof" = release ] && echo release || echo "$prof")
        echo "--- profile=$prof counters=$c"
        taskset -c 2 "./target/$dir/arena_backing" 2>/dev/null | grep -E "^  (heap|memfd) arena"
      done
    done


# **What the default interpolator buys, as a function of publish rate** (PROJECT §5 D5); pairs with `interp-cost`. Gates nothing.
interp-accuracy:
    cargo run --release -q -p tf_tree_bench --example interp_accuracy

# **The runtime path, as a node writes it**: `crates/tf_tree/examples/control_loop.rs`.
# Reports; gates nothing (PHASE1 §11.3's latency criteria need core-pinned hardware).
control-loop:
    cargo run --release -q -p tf_tree --features shm --example control_loop

# **One arena, two processes**: the publisher/consumer seam. Reports; fails if the consumer does.
two-processes:
    cargo run --release -q -p tf_tree --features shm --example two_processes

# **Is the C ABI's +101 ns on a shared arena the ABI, or the C++ caller?** Calls `tft_plan_at` from Rust on the same
# arena and stamps: near 302 it is the ABI, near 220 the C++ side, and `0022` is aimed at the wrong thing.
abi-attached:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -q --features abi-probe -p tf_tree_bench --bin abi_attached
    cargo build --profile embedder -q --features abi-probe -p tf_tree_bench --bin abi_attached
    cargo build --release -q --features shm -p tf_tree_bench --bin native_arena
    rt=$(mktemp -d /tmp/tft-abi-attached.XXXXXX); trap 'rm -rf "$rt"' EXIT
    export TF_TREE_RUNTIME_DIR="$rt" TF_TREE_NAME=abi_attached
    coproc OWNER { ./target/release/native_arena --name abi_attached --stream "$rt/fx.tfstream"; }
    read -r -u "${OWNER[0]}" line || { echo "the arena owner exited before it was ready" >&2; exit 1; }
    case "$line" in ready\ *) : ;; *) echo "unexpected owner greeting: $line" >&2; exit 1 ;; esac
    status=0
    echo "=== release profile (lto = \"thin\" — the boundary is ERASED) ==="
    taskset -c 2 ./target/release/abi_attached abi_attached || status=$?
    echo
    echo "=== embedder profile (lto = false — a REAL boundary) ==="
    taskset -c 2 ./target/embedder/abi_attached abi_attached --boundary-real || status=$?
    if [ -n "${OWNER[1]:-}" ]; then exec {OWNER[1]}>&- || true; fi
    wait "${OWNER_PID:-}" 2>/dev/null || true
    exit "$status"

# **PHASE2 §12's attach rows** (cold/warm attach p50, first access p99.9). The `population off` arm waits for 0022 B2-prime. Pinned.
attach-bench:
    cargo build --release -q --features shm -p tf_tree_bench --bin attach_bench
    taskset -c 2 ./target/release/attach_bench

# Run a gate recipe and read its exit code (0 PASS, 1 FAIL, 2 REFUSED) against POLICY `must-pass`, `may-refuse` or
# `must-refuse`; `scripts/gate-run.sh` is the only reader.
gate RECIPE POLICY:
    ./scripts/gate-run.sh "{{RECIPE}}" "{{POLICY}}"

# **PHASE5 §12 criterion 5: ingest throughput >= 10x real time** (`docs/decisions/0050` says what the ratio divides).
# The corpus is generated per run by `tf_tree_ingest::fixture`. Not PHASE4 §6.3's bag-replay criterion.
gate5:
    cargo build --release -q -p tf_tree_bench --bin ingest_throughput
    ./target/release/ingest_throughput --corpus target/gate5/corpus.mcap --gate

# **PHASE5 §12 criterion 2: `.tft` open under 10 ms for a 233 MB index** (gated vs reported: `docs/PHASE5.md` §12 criterion 2, §9.3).
# Fixtures are deleted first so the build under test wrote them, and after success to free disk; a non-zero exit leaves them.
gate2:
    cargo build --release -q --features shm -p tf_tree_bench --bin frozen_open
    rm -f target/gate2/index.tft target/gate2/small.tft
    ./target/release/frozen_open --tft target/gate2/index.tft \
        --small-tft target/gate2/small.tft --robots 64 --history 40 --gate
    rm -f target/gate2/index.tft target/gate2/small.tft

# **PHASE5 §12 criterion 4: 16 workers sharing one `.tft`, total Pss within 1.2x of one worker.** `--gate`: `docs/PHASE5.md` §12 criterion 4.
# The fixture is deleted first (a stale `--tft` would PASS) and must be large (S >= 74p). Needs ~340 MiB disk and ~1 GiB RAM.
gate4:
    cargo build --release -q --features shm -p tf_tree_bench --bin frozen_workers
    rm -f target/gate4/workers.tft
    ./target/release/frozen_workers --tft target/gate4/workers.tft --workers 1,16 --gate

# Gate 4's second arm, a CPython worker: reports, does not gate (`docs/PHASE5.md` §12 criterion 4, "Amendment — 1.024× is a statement about a Rust worker").
# Shares and deletes gate 4's `workers.tft`; `--release` because Pss fails on a debug build; `--py-worker` is explicit because the compiled default is the build machine's path.
gate4-python: py-setup
    cargo build --release -q --features shm -p tf_tree_bench --bin frozen_workers
    VIRTUAL_ENV=.venv .venv/bin/maturin develop --uv -q --release
    rm -f target/gate4/workers.tft
    ./target/release/frozen_workers --tft target/gate4/workers.tft --workers 1,16 \
        --python .venv/bin/python \
        --py-worker crates/tf_tree_bench/python/gate4_worker.py

# **PHASE4 §7 gate criterion 1: what the C ABI costs a caller.** Only the `embedder` profile gates (`docs/decisions/0023`, `ready`).
# Not in a workflow: a latency quotient whose stability on a hosted runner is unmeasured.
abi-cost:
    #!/usr/bin/env bash
    set -euo pipefail
    T="${CARGO_TARGET_DIR:-target}"
    cargo build --release -q -p tf_tree_c --features test-hooks --example abi_cost
    cargo build --profile embedder -q -p tf_tree_c --features test-hooks --example abi_cost
    cargo build --release -q -p tf_tree_bench --bin quiet_check
    "$T/release/quiet_check" before
    echo
    echo "=== release profile (lto = \"thin\" — the boundary is ERASED; contrast only) ==="
    taskset -c 2 "$T/release/examples/abi_cost" release
    echo
    echo "=== embedder profile (lto = false — a REAL boundary; THIS one gates) ==="
    set +e
    taskset -c 2 "$T/embedder/examples/abi_cost" embedder
    gate=$?
    set -e
    echo
    if ! "$T/release/quiet_check" after; then
      exit 2
    fi
    exit "$gate"

# The C ABI under Miri and ASan (PHASE4 §6.1, §7 gate 4). Appends to `$MIRIFLAGS`.
c-abi-check:
    MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-disable-isolation" cargo +nightly miri test \
        -p tf_tree_c -p tf_tree_core \
        --features tf_tree_c/test-hooks,tf_tree_core/miri-soft-float --test abi
    MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-disable-isolation" cargo +nightly miri test \
        -p tf_tree_c -p tf_tree_core \
        --features tf_tree_c/test-hooks,tf_tree_core/miri-soft-float --test live
    MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-disable-isolation" cargo +nightly miri test \
        -p tf_tree_c -p tf_tree_core \
        --features tf_tree_c/test-hooks,tf_tree_core/miri-soft-float --test publish
    MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-disable-isolation" cargo +nightly miri test \
        -p tf_tree_c -p tf_tree_core \
        --features tf_tree_c/test-hooks,tf_tree_c/bridge,tf_tree_core/miri-soft-float \
        --test bridge
    # ASan with `shm`: the only sanitizer on the shared arm (Miri cannot run the `memfd`/socket/owner-thread path, 0015).
    RUSTFLAGS=-Zsanitizer=address cargo +nightly test -p tf_tree_c \
        --features test-hooks,bridge,shm --target x86_64-unknown-linux-gnu -Zbuild-std

# **The committed C headers: drift check, then compile and run them** (gcc/clang, C11/C++17, `-Werror`; PHASE4 §6.2).
# Needs `cbindgen` (`cargo install cbindgen --locked --version 0.29.4`); the pin keeps output byte-identical.
c-header-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo xtask headers --check
    cargo build --release -q -p tf_tree_c --features test-hooks,bridge
    inc=crates/tf_tree_c/include
    lib=target/release/libtf_tree_c.a
    src=crates/tf_tree_c/tests/c/abi_smoke.c
    out=$(mktemp -d)
    trap 'rm -rf "$out"' EXIT
    cp "$src" "$out/smoke.cpp"
    for cc in "gcc -std=c11" "clang -std=c11" "g++ -std=c++17" "clang++ -std=c++17"; do
        in="$src"
        case "$cc" in g++*|clang++*) in="$out/smoke.cpp";; esac
        printf '%-22s ' "$cc"
        $cc -Wall -Wextra -Wpedantic -Werror -DTFT_HAVE_BRIDGE -I "$inc" \
            -o "$out/smoke" "$in" "$lib" -lpthread -ldl -lm
        "$out/smoke"
    done

# **The C++ wrapper across PHASE4 §6.2's full matrix (2 compilers x 2 standards x 2 error modes), each run, then ASan/UBSan.**
# Sophus is optional and its absence reported (§4.3 stride hazard); `just cpp-deps` fetches it.
cpp-check:
    ./crates/tf_tree_c/tests/cpp/run.sh

# **The CMake package, proved by a downstream consumer** (§4.4) reaching tf_tree only through `find_package(tf_tree CONFIG)`.
cmake-check:
    ./crates/tf_tree_c/tests/cmake_consumer/run.sh

# **The §7 gate-2 benchmark: the C++ wrapper against the raw C ABI**, gated at 2 %; also Eigen batch and Sophus stride rows. Pinned.
cpp-bench:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -q -p tf_tree_c --features test-hooks
    out=$(mktemp -d); trap 'rm -rf "$out"' EXIT
    sophus=""
    [ -f target/thirdparty/Sophus/sophus/se3.hpp ] && \
        sophus="-isystem target/thirdparty/Sophus -DSOPHUS_USE_BASIC_LOGGING"
    eigen=""
    for d in /usr/include/eigen3 /usr/local/include/eigen3 target/thirdparty/eigen; do
        [ -d "$d" ] && eigen="-isystem $d" && break
    done
    [ -n "$eigen" ] || { echo "cpp-bench: Eigen not found; run \`just cpp-deps\`" >&2; exit 1; }
    for mode in "" "-fno-exceptions"; do
        g++ -O2 -std=c++17 $mode -Wall -Wextra -Werror -I crates/tf_tree_c/include \
            $eigen $sophus -o "$out/bench" \
            crates/tf_tree_c/tests/cpp/bench.cpp target/release/libtf_tree_c.a \
            -lpthread -ldl -lm
        taskset -c 2 "$out/bench"
        echo
    done

# Fetch Eigen (if absent) and Sophus into target/thirdparty for `cpp-check`; not vendored.
cpp-deps:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p target/thirdparty
    if [ -d /usr/include/eigen3 ] || [ -d /usr/local/include/eigen3 ]; then
        echo "cpp-deps: Eigen already installed system-wide"
    elif [ -d target/thirdparty/eigen ]; then
        echo "cpp-deps: Eigen already fetched"
    else
        git clone -q -c advice.detachedHead=false --depth 1 --branch 3.4.0 \
            https://gitlab.com/libeigen/eigen.git target/thirdparty/eigen
        echo "cpp-deps: fetched Eigen 3.4.0"
    fi
    if [ -d target/thirdparty/Sophus ]; then
        echo "cpp-deps: Sophus already present"
        exit 0
    fi
    git clone -q -c advice.detachedHead=false --depth 1 --branch 1.22.10 \
        https://github.com/strasdat/Sophus.git target/thirdparty/Sophus
    echo "cpp-deps: fetched Sophus 1.22.10"

# **`tf_tree_ingest`'s feature axes, which `--workspace` compiles exactly one of** (`fixture` is off by default).
ingest-check:
    cargo clippy -p tf_tree_ingest --features fixture --all-targets -- -D warnings
    cargo nextest run -p tf_tree_ingest --features fixture
    cargo clippy -p tf_tree_ingest --no-default-features --all-targets -- -D warnings
    cargo nextest run -p tf_tree_ingest --no-default-features
    cargo clippy -p tf_tree_cli --all-targets -- -D warnings
    cargo nextest run -p tf_tree_cli
    cargo clippy -p tf_tree_cli --no-default-features --all-targets -- -D warnings
    cargo nextest run -p tf_tree_cli --no-default-features
    # The shipped CLI links both codecs, asserted on the dependency graph.
    cargo tree -q -p tf_tree_cli -e normal | grep -q ruzstd || \
        { echo "tf_tree_cli's default build has no zstd decoder: is 'compression' still in [features] default?"; exit 1; }
    cargo tree -q -p tf_tree_cli -e normal | grep -q lz4_flex || \
        { echo "tf_tree_cli's default build has no lz4 decoder: is 'compression' still in [features] default?"; exit 1; }
    # Negative direction: `tf_tree_ingest/fixture` fabricates recordings and must not reach the shipped binary via the `tf_tree_bench` edge.
    cargo tree -q -p tf_tree_cli -e normal --format "{p} [{f}]" | grep tf_tree_ingest | grep -q fixture && \
        { echo "tf_tree_cli's default build carries tf_tree_ingest/fixture: a dependency-level feature is travelling the tf_tree_bench edge"; exit 1; } || true

# **Rustdoc, with warnings denied — the docs.rs shop window.**
# docs.rs's configuration: the five publishable crates set `all-features` and `--cfg docsrs`; `tf_tree_bench` names `shm,embed-probe`. Not gated: `tf2` (`just tf2-check`), `tf_tree_py` (`just py-lint`), `tf_tree_tf2_sys`.
doc:
    RUSTDOCFLAGS='-D warnings --cfg docsrs' cargo doc --no-deps --all-features \
        -p tf_tree -p tf_tree_core -p tf_tree_math -p tf_tree_arena \
        -p tf_tree_ipc -p tf_tree_c -p tf_tree_cli -p tf_tree_ingest \
        -p tf_tree_bridge
    RUSTDOCFLAGS='-D warnings' cargo doc --no-deps -p tf_tree_bench \
        --features shm,embed-probe
    RUSTDOCFLAGS='-D warnings' cargo doc --no-deps -p xtask

# fmt, then one `clippy -D warnings` pass per feature configuration the workspace pass compiles out, behind the cheap audits;
# the `lint:` line is their order (`no-build-output` first, `unsafe-budget` last). `sbom` only runs, so the generator is exercised before a release.
lint: no-build-output no-conflict-markers py-compile evidence-audit artifact-versions sbom unsafe-budget
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy -p tf_tree_c --features bridge --all-targets -- -D warnings
    cargo clippy -p tf_tree_c --features test-hooks --all-targets -- -D warnings
    cargo clippy -p tf_tree_ingest --features fixture --all-targets -- -D warnings
    cargo clippy -p tf_tree_ingest --no-default-features --all-targets -- -D warnings
    cargo clippy -p tf_tree_cli --no-default-features --all-targets -- -D warnings
    cargo clippy -p tf_tree_core --features pure-hash --all-targets -- -D warnings
    cargo clippy -p tf_tree --features pure-hash --all-targets -- -D warnings
    cargo clippy -p tf_tree_core --features crash-points --all-targets -- -D warnings
    cargo clippy -p tf_tree_core --no-default-features --features crash-points --all-targets -- -D warnings

# fmt + clippy for `tf_tree_py`, which is workspace-excluded, so `cargo fmt --all` and every workspace command skip it.
# Needs only an interpreter (`.venv` wins, else `python3`), not headers; skips only with none.
py-compile:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo fmt --manifest-path crates/tf_tree_py/Cargo.toml -- --check
    if [ -x .venv/bin/python ]; then
        interpreter=$PWD/.venv/bin/python
    elif interpreter=$(command -v python3); then
        :
    else
        echo "py-compile: SKIPPED — no interpreter. Install python3, or run \`just py-setup\`." >&2
        exit 0
    fi
    PYO3_PYTHON=$interpreter cargo clippy \
        --manifest-path crates/tf_tree_py/Cargo.toml --all-targets -- -D warnings

# Format and auto-fix safe lint issues.
fmt:
    cargo fmt --all
    cargo fmt --manifest-path crates/tf_tree_py/Cargo.toml
    cargo clippy --workspace --all-targets --fix --allow-dirty -- -D warnings

# cargo-deny: advisories, licenses, bans, sources.
audit:
    cargo deny check

# **The MSRV floor, on the host rather than only in CI**: a `--locked` build of `--lib --bins` on the manifest's `rust-version`.
# No fallback to stable: a missing floor toolchain stops with the `rustup toolchain install` line. `ci.yml` runs this recipe.
msrv:
    #!/usr/bin/env bash
    set -euo pipefail
    want=$(grep -m1 '^rust-version' Cargo.toml | cut -d'"' -f2)
    test -n "$want" || { echo "no rust-version in Cargo.toml"; exit 1; }
    rustup toolchain list | grep -q "^$want" \
        || { echo "the floor is $want; install it: rustup toolchain install $want"; exit 1; }
    echo "==> building the workspace on the declared floor, $want"
    cargo "+$want" build --workspace --lib --bins --locked
    echo "==> every hand-written rust-version agrees with the workspace"
    rc=0
    for m in crates/*/Cargo.toml xtask/Cargo.toml; do
        got=$(grep -m1 '^rust-version *=' "$m" | cut -d'"' -f2 || true)
        [ -n "$got" ] || continue
        if [ "$got" != "$want" ]; then
            echo "$m: declares rust-version $got, workspace declares $want"
            rc=1
        fi
    done
    echo "==> the number is stated where a user reads it, and still agrees"
    for f in README.md SUPPORT.md CLAUDE.md crates/tf_tree/src/lib.rs \
             crates/tf_tree/README.md crates/tf_tree_core/README.md \
             crates/tf_tree_math/README.md crates/tf_tree_arena/README.md \
             crates/tf_tree_ipc/README.md; do
        if ! grep -qF "**$want**" "$f"; then
            echo "$f: does not state the MSRV as **$want**"
            rc=1
        fi
    done
    exit $rc

# One version across the repository, no document naming a recipe that is not there, no table row GFM would truncate.
# Enumerates `git ls-files`: `git add` a new document before running this.
artifact-versions:
    ./scripts/artifact-versions.py

# Run the benchmark suite and the go/no-go gate.
bench:
    cargo xtask bench-gate

# **What the `0036` receipt-time sampler costs a publisher.** Reports; gates nothing. Paired arms in one process,
# because two runs minutes apart drift more than the ~1.1 ns effect.
push-sampler-cost:
    cargo bench -p tf_tree_bench --bench push_sampler

# **`docs/PHASE5.md` §9's benchmark artifact**: `report/results.json`, `index.html` and the §9.3 provenance header.
# `--release` is required (debug marks timing rows UNAVAILABLE); unfair rows print UNAVAILABLE, and `TF_TREE_BENCH_FORCE=1` downgrades them to `indicative`.
bench-report *ARGS:
    cargo run --release -p tf_tree_bench --bin bench_report -- {{ARGS}}

# **The same report with the frozen backend (`shm`, Linux-only) compiled in**, the command the two `.tft` rows name
# (`report::tests::every_command_the_report_names_is_a_command_that_exists`); they still need a representative `.tft` and 16 physical cores.
bench-report-shm *ARGS:
    cargo run --release -p tf_tree_bench --features shm --bin bench_report -- {{ARGS}}

# **`docs/PHASE5.md` §9.2's two embedding measurements.** One is gated: two identical `#[inline(never)]` depth-3 lookups read off `[profile.embedder]`; the other is exploratory (API.md §2.3 item 2).
# `bench-check` and `bench-baseline-update` depend on this and read `target/embed-cost`; `EMBED_COST_KNOWN_COLLAPSED=1` is CI's disclosed escape.
embed-cost:
    #!/usr/bin/env bash
    set -euo pipefail
    out=target/embed-cost
    mkdir -p "$out"
    bin_dir="${CARGO_TARGET_DIR:-target}"
    cargo build -q --profile embedder -p tf_tree_bench --features embed-probe --bin embed_cost
    cargo build -q --release -p tf_tree_bench --features embed-probe --bin embed_cost
    # STRUCTURAL SELF-CHECK: if both columns collapse to one out-of-line symbol the quotient is 1.0 by construction; an empty subject set REFUSES.
    body_size() {
        nm --print-size --defined-only -C "$bin_dir/embedder/embed_cost" \
          | awk -v p="$1" '$4 == p { s = $2; found = 1 } END { if (!found) exit 1; print s }'
    }
    for sym in tf_tree_bench::embed::one tf_tree_core::bench_probe::depth3_lookup; do
        body_size "$sym" >/dev/null || {
            echo "embed-cost REFUSES: no symbol \`$sym\` in $bin_dir/embedder/embed_cost." >&2
            echo "  This check has lost its subject. Re-cut it against whatever the two" >&2
            echo "  columns are called now; do not delete it — an absent subject is" >&2
            echo "  indistinguishable from a passing check, which is the defect it exists" >&2
            echo "  to catch." >&2
            exit 1
        }
    done
    if [ "$(body_size tf_tree_bench::embed::one)" = "$(body_size tf_tree_core::bench_probe::depth3_lookup)" ]; then
        echo "embed-cost: both columns compiled to the same body, so the crate boundary" >&2
        echo "  is not this row's variable and the gate cannot fail." >&2
        echo "  Confirm with:" >&2
        echo "    nm -C --print-size --defined-only $bin_dir/embedder/embed_cost \\" >&2
        echo "      | grep -E 'embed::one\$|bench_probe::depth3_lookup\$'" >&2
        echo "    objdump -d -C $bin_dir/embedder/embed_cost   # both call one shared symbol" >&2
        echo "  The cause on 2026-08-29 was \`Plan::at_tagged\` interposed between" >&2
        echo "  \`Plan::at\` and the fold with no \`#[inline]\`, which makes both columns" >&2
        echo "  a stub around one out-of-line symbol in \`tf_tree_core\`. Two ways out and" >&2
        echo "  they are a trade, not a fix: mark \`at_tagged\` (read its doc comment" >&2
        echo "  first — the one measurement anybody has taken says it costs instructions," >&2
        echo "  and \`docs/API.md\` §2.3 prices the code-size half), or re-anchor the row" >&2
        echo "  on an entry point that is still inlinable across the boundary, which" >&2
        echo "  changes what \`docs/PHASE5.md\` §9.2 measures and is a decision record." >&2
        # `EMBED_COST_KNOWN_COLLAPSED` is the escape; delete this branch and `ci.yml`'s `env:` entry when the collapse is repaired.
        if [ "${EMBED_COST_KNOWN_COLLAPSED:-}" = "1" ]; then
            echo "" >&2
            echo "  EMBED_COST_KNOWN_COLLAPSED=1 is set, so this run continues." >&2
            echo "  READ THIS BEFORE READING THE NUMBERS BELOW: while the two bodies are" >&2
            echo "  identical the quotient is 1.0 by construction. A \`within\` verdict from" >&2
            echo "  this run is not evidence that the crate boundary is cheap; it is the" >&2
            echo "  absence of a measurement. \`Verdict::Over\` is unreachable." >&2
        else
            echo "" >&2
            echo "  Set EMBED_COST_KNOWN_COLLAPSED=1 to run anyway with that disclosure" >&2
            echo "  printed; do not delete this check." >&2
            exit 1
        fi
    fi
    taskset -c 2 "$bin_dir/embedder/embed_cost" --json "$out/embedder.json"
    taskset -c 2 "$bin_dir/release/embed_cost" --json "$out/release.json"
    "$bin_dir/release/embed_cost" --compare "$out"

# **fmt / clippy / tests for the default-off `embed-probe` configuration**, which `just test` compiles out;
# a new `embed-probe`-only test target joins this list.
embed-cost-check:
    cargo fmt --check -p tf_tree_core -p tf_tree_bench
    cargo clippy -p tf_tree_core --features bench-probe --all-targets -- -D warnings
    cargo clippy -p tf_tree_bench --features embed-probe --all-targets -- -D warnings
    cargo nextest run -p tf_tree_bench --features embed-probe -E 'test(/embed/)'

# **`docs/PHASE5.md` §10's regression gate**: regenerates the report and compares it to `crates/tf_tree_bench/baseline/results.json`; fails on a withdrawn claim, dropped row, changed arena layout or a directional number past its slack.
# `--embed-cost` must match `bench-baseline-update`'s flags (the status comparison is one-directional).
bench-check: embed-cost
    cargo run --release -p tf_tree_bench --bin bench_report -- \
        --out target/bench-report \
        --embed-cost target/embed-cost \
        --check-baseline crates/tf_tree_bench/baseline/results.json

# Regenerate the committed baseline. **Run deliberately; commit the diff with the change that causes it.**
# Must take the same `--embed-cost` as `bench-check`. `index.html` is not committed.
bench-baseline-update: embed-cost
    cargo run --release -p tf_tree_bench --bin bench_report -- --out target/bench-report \
        --embed-cost target/embed-cost
    cp target/bench-report/results.json crates/tf_tree_bench/baseline/results.json

# --- The performance suite (exploratory; NOT the `bench-check` gate) ---------
# None of these feed `bench-check`: this host fails `Fitness::probe`, so timing rows here are indicative.

# List the workload catalogue: what each named load is and why it is there.
workloads:
    cargo run --release -p tf_tree_bench --features shm --bin contended_scaling -- --list

# PHASE1 §11.2's read-scaling row: N readers x M writers on one arena, pinned. Refuses to run on a busy machine (`mp-bench`'s reason).
contended-scaling *ARGS:
    cargo build --release --features shm -p tf_tree_bench --bins
    taskset -c 0-7 ./target/release/contended_scaling {{ARGS}}

# Extreme-scale sweep: width, depth, ring size, publish fan-out, and the limits. Single-process; no ROS or shm.
scale-sweep *ARGS:
    cargo run --release -p tf_tree_bench --bin scale_sweep -- {{ARGS}}

# Long-duration steady state. EXITS NON-ZERO if p99.9 drifts more than 3x, RSS grows past 8 MiB, or the rings never lapped (PHASE2 §11.4).
soak *ARGS:
    cargo run --release -p tf_tree_bench --bin soak -- {{ARGS}}

# The overnight soak: 30 minutes on fleet_16 (about 180 ring laps), one snapshot a minute.
soak-long:
    cargo run --release -p tf_tree_bench --bin soak -- \
        --workload fleet_16 --duration 30m --interval 60s \
        --json target/bench-runs/soak-long.json

# --- The A/B loop: did that change help? ------------------------------------
# Every harness takes `--json <path>`; direction and slack travel in the file, never inferred from a key name.

# Run the light half of the suite and write target/bench-runs/<sha>[-dirty].json.
bench-run workload="robot":
    #!/usr/bin/env bash
    set -euo pipefail
    sha=$(git rev-parse --short HEAD)
    if [ -n "$(git status --porcelain)" ]; then sha="$sha-dirty"; fi
    out="target/bench-runs/$sha"
    mkdir -p "$out"
    cargo build --release --features shm -p tf_tree_bench --bins
    taskset -c 0-7 ./target/release/contended_scaling \
        --workload {{workload}} --seconds 3 --readers 1,2,4,8 --writers 0,4 \
        --json "$out/contended_scaling.json"
    ./target/release/scale_sweep --json "$out/scale_sweep.json"
    echo
    echo "wrote $out/{contended_scaling,scale_sweep}.json"

# Compare two run files; non-zero exit means a metric regressed past its tolerance.
bench-ab a b:
    cargo run --release -p tf_tree_bench --bin bench_ab -- {{a}} {{b}}

# --- Profiling: where does the time actually go? ----------------------------

# Sampling profile of a workload, for a flamegraph. Needs perf_event_paranoid <= 1 (the recipe prints the fix); uses the `profiling` profile (release + debuginfo).
profile workload="fleet_16" seconds="20":
    #!/usr/bin/env bash
    set -euo pipefail
    paranoid=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 4)
    if [ "$paranoid" -gt 1 ]; then
        echo "perf_event_paranoid is $paranoid; perf cannot sample. Either:" >&2
        echo "  sudo sysctl kernel.perf_event_paranoid=1" >&2
        echo "or use the simulated, permission-free path:" >&2
        echo "  just profile-cachegrind {{workload}}" >&2
        exit 1
    fi
    command -v perf >/dev/null || { echo "perf is not installed" >&2; exit 1; }
    cargo build --profile profiling -p tf_tree_bench --bin soak
    mkdir -p target/profile
    perf record -F 999 -g --call-graph dwarf -o target/profile/perf.data -- \
        ./target/profiling/soak --workload {{workload}} \
            --duration {{seconds}}s --interval {{seconds}}s
    perf script -i target/profile/perf.data > target/profile/out.perf
    echo "wrote target/profile/out.perf — fold it with inferno-collapse-perf or stackcollapse-perf"

# Per-line instruction counts and cache misses over a workload (cachegrind, ~50x slower than native). No privileges needed.
profile-cachegrind workload="robot":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v valgrind >/dev/null; then
        echo "valgrind is not installed on this host, so this recipe cannot run." >&2
        echo "Two ways past it:" >&2
        echo "  sudo apt-get install valgrind      # then re-run this recipe" >&2
        echo "  just profile-lookup                # per-line, in docker/tf2, which ships valgrind" >&2
        echo "The container path is pinned to \`footprint\`'s one query; this recipe is" >&2
        echo "the one that takes a --workload. They are not substitutes for each other." >&2
        exit 1
    fi
    cargo build --profile profiling -q -p tf_tree_bench --bin soak
    mkdir -p target/profile
    valgrind --tool=cachegrind --branch-sim=yes --cache-sim=yes \
        --cachegrind-out-file=target/profile/cg.out \
        ./target/profiling/soak --workload {{workload}} --duration 4s --interval 2s \
        >/dev/null 2>&1 || true
    cg_annotate --show=Ir,Bcm,D1mr --sort=Ir --auto=yes target/profile/cg.out

# **The §9.2 artifact with the tf2 columns compiled in, and its own baseline** (`results-tf2.json`). Container-only.
# `lookup_ratio_vs_tf2` resolves only here (`0025`); a `tf2`-cut baseline would fail `bench-check` on hosts without ROS 2.
tf2-bench-report *ARGS:
    ./docker/tf2/run.sh 'cargo run --release -p tf_tree_bench --features tf2 --bin bench_report -- {{ARGS}}'

# The tf2-side regression gate. Container-only.
# Its `arena_memory_floor.idle_arena_resident_bytes` bound is wide: the baseline predates `0021` step 2 (value `2408448`), so it tightens only after `tf2-bench-baseline-update`.
tf2-bench-check:
    ./docker/tf2/run.sh 'cargo run --release -p tf_tree_bench --features tf2 --bin bench_report -- \
        --out target/tf2-bench-report \
        --check-baseline crates/tf_tree_bench/baseline/results-tf2.json'

# Regenerate the tf2-side baseline; run deliberately, and commit the diff with its cause.
tf2-bench-baseline-update:
    ./docker/tf2/run.sh 'cargo run --release -p tf_tree_bench --features tf2 --bin bench_report -- \
        --out target/tf2-bench-report'
    cp target/tf2-bench-report/results.json crates/tf_tree_bench/baseline/results-tf2.json

# **Which consumer build does the gated ratio speak for? Both.** Same paired harness under `[profile.release]` (`lto = "thin"`, this workspace) and `[profile.embedder]` (cargo's release defaults, a consumer's build).
# Read the tf2 column too: it goes through a C++ shim and should barely move, else the runs are not comparable. Pinned; not gated (`runstore::BUILD_CRITICAL_FACTS` refuses cross-profile comparison).
tf2-ratio-profiles:
    ./docker/tf2/run.sh 'set -euo pipefail; \
        cargo build --release -q -p tf_tree_bench --features tf2 --bin tf2_ratio; \
        cargo build --profile embedder -q -p tf_tree_bench --features tf2 --bin tf2_ratio; \
        echo "=== [profile.release] — lto = \"thin\": THIS workspace, not a consumer ==="; \
        taskset -c 2 ./target/tf2-docker/release/tf2_ratio; \
        echo; \
        echo "=== [profile.embedder] — lto = false: cargo release defaults, what a consumer gets ==="; \
        taskset -c 2 ./target/tf2-docker/embedder/tf2_ratio'

# fmt + clippy + unit tests for the tf2 bridge, in the container. `lint` and `test` cannot see it.
# `tf_tree_tf2_sys` is workspace-excluded (`--manifest-path`); the `tf_tree_c --features bridge,shm` row is the set `ros/build.sh` builds, on this image's toolchain.
tf2-check:
    ./docker/tf2/run.sh 'set -euo pipefail; \
        cargo fmt --manifest-path crates/tf_tree_tf2_sys/Cargo.toml -- --check; \
        cargo clippy --manifest-path crates/tf_tree_tf2_sys/Cargo.toml --all-targets -- -D warnings; \
        cargo nextest run --manifest-path crates/tf_tree_tf2_sys/Cargo.toml --release; \
        cargo clippy -p tf_tree_bench --features tf2 --all-targets -- -D warnings; \
        cargo nextest run -p tf_tree_bench --features tf2 --release --lib --no-tests=pass; \
        cargo clippy -p tf_tree_c --features bridge,shm --all-targets -- -D warnings'

# Build ros/tf_tree_ros (PHASE4 §5) in the container. Nothing on the host can; this and `ros-test` are the package's whole gate (see `ros/build.sh`).
ros-build:
    ./docker/tf2/run.sh './ros/build.sh'

# Build ros/tf_tree_ros and run its ctests (§6.3's QoS regression needs a real DDS). The only gate this package has.
ros-test:
    ./docker/tf2/run.sh './ros/build.sh --test'

# **`docs/PHASE5.md` §9.1's end-to-end comparison over a real DDS**: N `tf2_ros::TransformListener` consumers against one publisher, and the same queries through the ingest bridge (four arms, 0015).
# Env: WORKLOAD, CONSUMERS, SECONDS_MEASURED, WARMUP, HZ, BRIDGE_LINGER, TF_TREE_NAME.
dds-bench *ENV:
    ./docker/tf2/run.sh './ros/build.sh && {{ENV}} ./ros/dds_bench.sh'

# The tf2::BufferCore differential, in a ROS 2 container (first run builds the image). Everything below is container-only.
tf2-differential:
    ./docker/tf2/run.sh 'cargo test -p tf_tree_bench --features tf2 --release --test differential -- --nocapture'

# The same differential, over a real recorded /tf stream (`testdata/tfstream/ATTRIBUTION.md`).
tf2-replay:
    ./docker/tf2/run.sh 'cargo test -p tf_tree_bench --features tf2 --release --test replay -- --nocapture'

# Head-to-head performance against tf2; indicative unless pinned (docs/benchmarks/tf2.md).
tf2-bench:
    ./docker/tf2/run.sh 'cargo bench -p tf_tree_bench --features tf2 --bench tf2_compare'

# Concurrent read scaling at 1/2/4/8 threads, both engines interleaved (p50/p99/p99.9). RUN ON AN IDLE MACHINE. `TF2_WRITERS=N` adds writers on edges the query does not traverse (PHASE1 §11.2).
tf2-scaling *ENV:
    ./docker/tf2/run.sh '{{ENV}} cargo run -p tf_tree_bench --features tf2 --release --bin tf2_scaling'

# Instructions per `Ingest::offer`: cachegrind, N=0 baseline subtracted; exact under load, ~50x slower.
# The sweep is the measurement: `edges=1` is the control and name style is swept; cache geometry is pinned.
bridge-footprint:
    ./docker/tf2/run.sh 'set -e; cargo build --release -q -p tf_tree_bridge --example offer_cost; \
        B=./target/tf2-docker/release/examples/offer_cost; \
        for m in declared undeclared regressing; do \
          for e in 1 20 100; do \
            for st in short ros; do \
              for n in 0 200000; do \
                valgrind --tool=cachegrind --cache-sim=yes --branch-sim=yes \
                  --I1=32768,8,64 --D1=32768,8,64 --LL=33554432,16,64 \
                  --cachegrind-out-file=/dev/null $B $m $n $e $st 2>&1 \
                  | grep -E "I refs|D1  misses|Mispredicts" | tr -s " " \
                  | sed "s|^|[$m e=$e $st n=$n] |"; \
              done; done; done; done'

# Wall-clock cost of one `tft_bridge_offer` through the C ABI. RUN ON AN IDLE MACHINE; indicative, never `measured`.
bridge-cost:
    cargo build --release -q -p tf_tree_c --features bridge --example bridge_cost
    taskset -c 2 ./target/release/examples/bridge_cost

# Memory footprint and computation-per-lookup vs tf2 (docs/benchmarks/tf2.md); cachegrind/memcheck are exact under load, ~50x slower.
# Each mode runs in its own process so freed chunks cannot leak between engines.
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

# Line-level profile of the lookup hot path (docs/benchmarks/tf2.md), simulated; `profiling` profile because `fold_at` inlines the chain.
profile-lookup n="200000":
    ./docker/tf2/run.sh 'set -e; \
        cargo build --profile profiling -q -p tf_tree_bench --features tf2 --bin footprint; \
        B=./target/tf2-docker/profiling/footprint; \
        valgrind --tool=cachegrind --branch-sim=yes --cache-sim=yes \
            --cachegrind-out-file=/tmp/cg.out $B lookup-tf_tree {{n}} >/dev/null 2>&1; \
        cg_annotate --show=Ir,Bcm,D1mr --sort=Ir --auto=yes /tmp/cg.out'

# ThreadSanitizer over the concurrent read path (PHASE3 §7.3): real threads on real code, complementing `just loom`;
# it underwrites `tf_tree_py`'s `gil_used = false`. `-Zbuild-std` so std is instrumented.
tsan:
    RUSTFLAGS="-Zsanitizer=thread" \
    cargo +nightly test -Zbuild-std --target x86_64-unknown-linux-gnu \
        -p tf_tree --features shm --test tsan --release

# --- Phase 2: shared memory (Linux only; `shm` is off by default; no container needed) ---

# Multi-process gate: a second process maps the same arena and must answer bit-identically.
shm-test:
    cargo build --features shm -p tf_tree_bench --bin shm_child
    cargo nextest run -p tf_tree_bench --features shm --test multiprocess
    cargo clippy -p tf_tree_bench --features shm --bin owner_migration --all-targets -- -D warnings
    cargo nextest run -p tf_tree_bench --features shm --bin owner_migration

# N reader processes on one shared arena, plus the memory that sharing saves.
# RUN THIS ON AN IDLE MACHINE — the 8-process row oversubscribes 4 cores 2:1.
shm-scaling:
    cargo build --release --features shm -p tf_tree_bench --bins
    ./target/release/shm_scaling

# Multi-process NODE evaluation: N consumers at a fixed rate with a live publisher. Refuses to run on a busy machine; `TF_TREE_BENCH_FORCE=1` overrides.
mp-bench:
    cargo build --release --features shm -p tf_tree_bench --bins
    taskset -c 0-7 ./target/release/mp_bench tf_tree

# The same against tf2, in the container. The tf2 column is a FLOOR: a private BufferCore per consumer, no transport.
mp-bench-tf2:
    ./docker/tf2/run.sh 'set -e; cargo build --release --features "shm tf2" -p tf_tree_bench --bins; \
        ./target/tf2-docker/release/mp_bench tf_tree; \
        echo; ./target/tf2-docker/release/mp_bench tf2'

# fmt + clippy + tests for everything behind `shm`, which `lint` and `test` do not compile (`tf_tree` + `tf_tree_ipc` only meet under it, 0005).
shm-check:
    cargo clippy -p tf_tree_arena --features shm --all-targets -- -D warnings
    cargo clippy -p tf_tree --features shm --all-targets -- -D warnings
    cargo clippy -p tf_tree --features shm,unstable --all-targets -- -D warnings
    cargo clippy -p tf_tree --features shm,test-hooks,unstable --all-targets -- -D warnings
    cargo clippy -p tf_tree_ipc --all-targets -- -D warnings
    cargo clippy -p tf_tree_bench --features shm --all-targets -- -D warnings
    cargo clippy -p tf_tree_cli --features shm --all-targets -- -D warnings
    # `bridge,shm` together, which no other recipe builds (`0015`). Linux-only, so not `test-rust`.
    cargo clippy -p tf_tree_c --features bridge,shm --all-targets -- -D warnings
    cargo nextest run -p tf_tree_c --features bridge,shm
    cargo build --features shm -p tf_tree_bench --bin shm_child
    cargo build --features shm -p tf_tree_bench --bin fork_child
    cargo nextest run -p tf_tree_bench --features shm --test multiprocess
    cargo nextest run -p tf_tree_bench --features shm --lib
    cargo nextest run -p tf_tree_bench --features shm --bins
    cargo nextest run -p tf_tree_bench --features shm --test gate4
    # PHASE5 §12 gates 2 and 4: exit status and refusals through the shipped binary; fixtures go under the cargo target dir, not tmpfs.
    cargo nextest run -p tf_tree_bench --features shm --test gate2
    cargo clippy -p tf_tree_bench --features abi-probe --all-targets -- -D warnings
    cargo nextest run -p tf_tree_bench --features shm --test fork
    cargo clippy -p tf_tree_bench --features shm,bridge --all-targets -- -D warnings
    cargo build --features shm,bridge -p tf_tree_bench --bin fork_child
    cargo nextest run -p tf_tree_bench --features shm,bridge --test fork
    cargo nextest run -p tf_tree_bench --features shm --test population
    cargo nextest run -p tf_tree_cli --features shm --lib
    cargo nextest run -p tf_tree_cli --features shm --test attach
    cargo nextest run -p tf_tree_cli --features shm --test web
    cargo nextest run -p tf_tree_cli --features shm --test doctor_frozen
    cargo nextest run -p tf_tree_cli --features shm --test doctor_runtime_dir
    cargo nextest run -p tf_tree_cli --features shm --test replay_bit_identity
    cargo nextest run -p tf_tree_cli --features shm --test runbook
    cargo nextest run -p tf_tree_arena --features shm
    cargo nextest run -p tf_tree --features shm,unstable --test frozen
    # `docs/PHASE2.md` §11.3's crash matrix; needs `crash-points` or the armed child never dies. `prlimit --core=1:1 --` (`0057` step 4): a pipe `core_pattern` dump could decide the reaping tests.
    prlimit --core=1:1 -- cargo nextest run -p tf_tree --features shm,unstable,crash-points --test rendezvous
    cargo clippy -p tf_tree --features shm,unstable,crash-points --all-targets -- -D warnings
    cargo nextest run -p tf_tree --features shm --test owned_writer
    cargo nextest run -p tf_tree --features shm --test error_payloads
    cargo nextest run -p tf_tree --features shm --lib
    cargo nextest run -p tf_tree_bench --features shm --release --test torture
    cargo clippy -p tf_tree_ingest --features shm --all-targets -- -D warnings
    cargo nextest run -p tf_tree_ingest --features shm
    cargo nextest run -p tf_tree_ipc

# **`docs/PHASE2.md` §11.4's `shm_torture`** (nightly per `docs/PHASE5.md` §10): N processes doing random attach/detach/claim/reap/push/lookup while the driver `SIGKILL`s one several times a second.
# Override e.g. `just shm-torture "--duration 60s --children 4"`; `--release` for real interleavings. `prlimit --core=1:1 --` on all three torture recipes (`0057` step 4).
shm-torture *ARGS="--duration 30m --children 6 --kill-hz 6":
    cargo build --release --features shm -p tf_tree_bench --bin shm_torture
    prlimit --core=1:1 -- ./target/release/shm_torture {{ARGS}}

# **§11.4's "a random crash point armed in 10% of children"** (`docs/PHASE2.md` §11.3 x §11.4): arms a random site from `tf_tree_core::crash::SITES` and `tf_tree::CRASH_SITES`, so kills land at named instructions.
# Needs the `crash-points` feature; the binary refuses `armed 0` and `armed N, aborted 0`. Nightly (`nightly.yml`'s `crash-points` job); `prlimit` as in `shm-torture`.
shm-torture-crash-points *ARGS="--duration 5m --children 10 --kill-hz 2":
    cargo build --release --features shm,crash-points -p tf_tree_bench --bin shm_torture
    prlimit --core=1:1 -- ./target/release/shm_torture --crash-points {{ARGS}}

# **`docs/PHASE2.md` §12.2's two ownership-migration rows and §12.3 gate 4b**: five processes, five migrations (a serve-only owner, a never-killed writer, an heir running §3.5's caller-driven trigger, read-only readers).
# Exits non-zero on FAIL and separately on INVALID (a starved writer). Run on an idle machine; `--repeat` buys tail samples. `${CARGO_TARGET_DIR:-target}` so a moved target dir does not exec a stale binary.
owner-migration *ARGS:
    cargo build --release -q --features shm -p tf_tree_bench --bin owner_migration
    "${CARGO_TARGET_DIR:-target}/release/owner_migration" {{ARGS}}

# **The torture harness's own gate**, seconds long: one run with a corrupt-transform child another process must catch, one clean run that validated thousands of reads.
shm-torture-self-test:
    cargo nextest run -p tf_tree_bench --features shm --release --test torture

# **`docs/PHASE5.md` §5.1's NORMATIVE CI test, and §13's box 4**: the library opens no network socket. `scripts/no-network.sh` has the PROVES / DOES NOT PROVE header.
# Binaries run outside nextest with `--test-threads 1`: six rendezvous tests share a runtime directory.
no-network:
    ./scripts/no-network.sh

# **§11.4's "run it under ASan"**, a short run: ASan follows `fork`/`exec`, so children are instrumented, and Miri cannot reach the multi-process `unsafe`.
# `-Zbuild-std` so std is instrumented (minutes); `detect_leaks=0` because a `SIGKILL`ed child is defined to leak. `prlimit` as in `shm-torture`.
shm-torture-asan *ARGS="--duration 120s --children 4 --kill-hz 4":
    RUSTFLAGS="-Zsanitizer=address" ASAN_OPTIONS=detect_leaks=0 \
    prlimit --core=1:1 -- \
    cargo +nightly run -Zbuild-std --target "$(rustc -vV | sed -n 's/^host: //p')" \
        --release --features shm -p tf_tree_bench --bin shm_torture -- {{ARGS}}

# The zero-config rendezvous end to end: a foreign process calls
# `tf_tree::open()`, joins a served arena, and reads the same transform.
shm-rendezvous:
    # `test-hooks` and `unstable` stage states no outside process can reach; without them the recipe silently runs fewer tests. `prlimit` as in `shm-check`.
    prlimit --core=1:1 -- cargo nextest run -p tf_tree --features shm,test-hooks,unstable --test rendezvous

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

# **The memory comparison with no binding on either side**: a C++ program linking only `libtf2` against `footprint`'s `mem-tf_tree`, by `mallinfo2` and Pss. Needs no idle machine; refuses a quotient if the arms stored different sample counts.
tf2-native-footprint:
    ./docker/tf2/run.sh 'bash docker/tf2/native_footprint.sh'

# **The depth-3 ratio with no Rust binding on either arm**: tf2 native C++ against `tf_tree` through its C ABI as a shared library, interleaved in one process; `native_arena` serves the fixture (D18: `tft_tree_open` attaches, cannot create).
# Brackets `tf2-bench-check`'s row rather than replacing it (`docs/benchmarks/tf2.md`); not gated.
tf2-native-ratio *ARGS:
    ./docker/tf2/run.sh 'bash docker/tf2/native_ratio.sh {{ARGS}}'

# **Splitting `tf2-native-ratio`'s +52% into the memfd mapping and the shared-library boundary**: the middle arm, native Rust on the same `memfd` backing. Paired and interleaved; host-only, needs `shm`.
# Not gated: the boundary half is a subtraction against another run's figure, and the tool prints which row is which.
abi-split:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -q --features shm -p tf_tree_bench --bin arena_backing --bin native_arena
    rt=$(mktemp -d /tmp/tft-abi-split.XXXXXX); trap 'rm -rf "$rt"' EXIT
    export TF_TREE_RUNTIME_DIR="$rt" TF_TREE_NAME=abi_split
    coproc OWNER { ./target/release/native_arena --name abi_split --stream "$rt/fx.tfstream"; }
    read -r -u "${OWNER[0]}" line || { echo "the arena owner exited before it was ready" >&2; exit 1; }
    case "$line" in ready\ *) : ;; *) echo "unexpected owner greeting: $line" >&2; exit 1 ;; esac
    status=0
    ./target/release/arena_backing --attach abi_split || status=$?
    # `|| true`: bash unsets the fd array once the coproc has exited, and `set -e` would skip `exit "$status"`.
    if [ -n "${OWNER[1]:-}" ]; then exec {OWNER[1]}>&- || true; fi
    wait "${OWNER_PID:-}" 2>/dev/null || true
    exit "$status"

# Python bindings (docs/PHASE3.md). `tf_tree_py` is workspace-excluded (libpython); interpreters come from uv (3.14 GIL, 3.14t free-threaded, §7.3).

# Clean clone -> a Python REPL with the extension installed, verified end to end.
# The last step runs `README.md`'s own snippet and compares its output with the snippet's `# ->` marker. Depends on `py-setup`, not a lighter venv: later recipes assume its venvs.
quickstart: py-setup
    VIRTUAL_ENV=.venv .venv/bin/maturin develop --uv -q
    .venv/bin/python scripts/quickstart_smoke.py
    @echo ""
    @echo "==> next: .venv/bin/python   (the extension is installed in that interpreter)"

# Create both venvs and install the toolchain. `--allow-existing` because `uv venv` errors on an existing directory and `quickstart` depends on this.
py-setup:
    uv python install 3.14 3.14t
    uv venv --python 3.14 --allow-existing .venv
    VIRTUAL_ENV=.venv uv pip install -q maturin numpy pytest ruff pyright
    uv venv --python 3.14t --allow-existing .venv-t
    VIRTUAL_ENV=.venv-t uv pip install -q maturin numpy pytest

# Build the extension into the GIL venv and run the suite. ~5 s of it is one test waiting out `DEFAULT_OPEN_TIMEOUT` in an `open` meant to fail.
py-test:
    VIRTUAL_ENV=.venv .venv/bin/maturin develop --uv -q
    cargo build -p tf_tree --features shm --bin tf_tree_rendezvous_child
    .venv/bin/python -m pytest tests/python -q

# The same on the free-threaded interpreter — §7.3's requirement.
py-test-freethreaded:
    VIRTUAL_ENV=.venv-t PYO3_PYTHON=$PWD/.venv-t/bin/python .venv-t/bin/maturin develop --uv -q
    cargo build -p tf_tree --features shm --bin tf_tree_rendezvous_child
    .venv-t/bin/python -m pytest tests/python -q

# **`docs/PHASE3.md` §12.2 criterion 4 / §7.3's scaling test**: >= 6x from 1 to 8 threads on `3.14t`, and on the GIL build for batches above the release threshold. Figures live only in `docs/benchmarks/EVIDENCE.md`.
# `--release` because a debug build reads ~6x worse and moves the curve. `.venv-t` is the free-threaded half (`py-thread-scaling-gil` the other). `--serialize` is the falling-curve control; `--gate --serialize` is refused.

# `plan.at` on 1/2/4/8 threads under `python3.14t` — PHASE3 §12.2 criterion 4.
py-thread-scaling *ARGS:
    VIRTUAL_ENV=.venv-t PYO3_PYTHON=$PWD/.venv-t/bin/python .venv-t/bin/maturin develop --uv -q --release
    .venv-t/bin/python crates/tf_tree_bench/python/thread_scaling.py {{ARGS}}

# **The GIL half of the same criterion**: same script, defaults and `--release`. `--call at_into` avoids the per-call (N,4,4) allocation under the GIL; `--gate --call at_into` is refused because §7.3 names `plan.at`.
py-thread-scaling-gil *ARGS:
    VIRTUAL_ENV=.venv PYO3_PYTHON=$PWD/.venv/bin/python .venv/bin/maturin develop --uv -q --release
    .venv/bin/python crates/tf_tree_bench/python/thread_scaling.py {{ARGS}}

# fmt + lint for both languages of the binding, plus the Rust half's rustdoc.
py-lint: py-compile
    # Rustdoc of `tf_tree_py`, which `just doc` cannot name (workspace-excluded); the venv's interpreter avoids thrashing PyO3's target directory.
    PYO3_PYTHON=$PWD/.venv/bin/python RUSTDOCFLAGS="-D warnings" cargo doc \
        --manifest-path crates/tf_tree_py/Cargo.toml --no-deps
    .venv/bin/ruff check python tests/python crates/tf_tree_bench/python scripts
    .venv/bin/ruff format --check python tests/python crates/tf_tree_bench/python scripts
    .venv/bin/pyright python
    .venv/bin/pyright tests/python/typecheck_stamps.py

# Build a release wheel (it does not install; `just quickstart` does that). `py-mp-bench` and `py-vs-tf2` unpack its output into a container.
py-wheel:
    rm -f crates/tf_tree_py/target/wheels/transform_tree-*.whl
    VIRTUAL_ENV=.venv .venv/bin/maturin build --release

# N Python consumer nodes on one shared arena against N private `tf2_ros` buffers (PHASE2 §12.4, PHASE3 §12.1): the deployment comparison. RUN THIS ON AN IDLE MACHINE.
py-mp-bench:
    just py-wheel
    ./docker/tf2/run.sh 'set -e; \
        rm -rf target/pywheel && mkdir -p target/pywheel; \
        python3 -c "import zipfile,glob; w=sorted(glob.glob(\"crates/tf_tree_py/target/wheels/transform_tree-*-cp314-*.whl\")); assert len(w)==1, w; print(\"unpacking\", w[0]); zipfile.ZipFile(w[0]).extractall(\"target/pywheel\")"; \
        PYTHONPATH=target/pywheel:$PYTHONPATH python3 crates/tf_tree_bench/python/mp_compare.py'

# tf_tree's Python API against tf2_ros's in the ROS container (PHASE3 §12.1): a host-built cp314 wheel, tf2 fed its BufferCore directly (no DDS).
py-vs-tf2:
    just py-wheel
    ./docker/tf2/run.sh 'set -e; \
        rm -rf target/pywheel && mkdir -p target/pywheel; \
        python3 -c "import zipfile,glob; w=sorted(glob.glob(\"crates/tf_tree_py/target/wheels/transform_tree-*-cp314-*.whl\")); assert len(w)==1, w; print(\"unpacking\", w[0]); zipfile.ZipFile(w[0]).extractall(\"target/pywheel\")"; \
        PYTHONPATH=target/pywheel:$PYTHONPATH python3 crates/tf_tree_bench/python/tf2_ros_compare.py'

# Build, verify and package the CLI for one target (`docs/PHASE5.md` §10). `release.yml` calls this per matrix row (and lists `justfile` in its `pull_request` trigger).
# The binary is executed before it is packaged: `--version` against the workspace number rejects a wrong-architecture, truncated or stale build.
GLIBC_FLOOR := "2.34"

release-archive TARGET:
    #!/usr/bin/env bash
    set -euo pipefail
    target="{{ TARGET }}"
    out_dir="${CARGO_TARGET_DIR:-target}"
    version="$(cargo pkgid -p tf_tree_cli | sed 's/.*[@#]//')"
    name="tf_tree-v${version}-${target}"
    staging="${out_dir}/release-staging"
    stage="${staging}/${name}"

    host_arch="$(uname -m)"
    want_arch="${target%%-*}"
    if [ "${host_arch}" != "${want_arch}" ]; then
        echo "::error::this host is ${host_arch}; ${target} needs a ${want_arch} runner." >&2
        echo "  This recipe verifies the artifact by running it, so an emulated" >&2
        echo "  execution would certify a binary nothing native has checked." >&2
        exit 1
    fi

    rustup target add "${target}" >/dev/null 2>&1 || true
    cargo build --locked --release -p tf_tree_cli --bin tf_tree \
        --features shm --target "${target}"

    bin="${out_dir}/${target}/release/tf_tree"
    [ -f "${bin}" ] || { echo "no binary at ${bin}" >&2; exit 1; }

    if ! "${bin}" --version >/dev/null 2>&1; then
        echo "::error::${bin} does not execute on this host." >&2
        exit 1
    fi
    got="$("${bin}" --version)"
    want="tf_tree ${version}"
    if [ "${got}" != "${want}" ]; then
        echo "::error::${bin} reports '${got}', workspace version is '${want}'" >&2
        exit 1
    fi
    echo "  verified: ${got} (${target}, native ${host_arch})"

    # The glibc floor is gated: `release.yml`'s notes, `README.md` and `docs/PHASE5.md` §10 quote it (ROS 2 Humble is glibc 2.35). Raising it is a documentation change.
    case "${target}" in
      *-musl)
        if command -v ldd >/dev/null 2>&1 && ldd "${bin}" 2>&1 | grep -qv 'statically linked'; then
            echo "::error::${target} is not statically linked" >&2
            exit 1
        fi
        echo "  glibc floor: none (static)" ;;
      *-gnu)
        floor="$(objdump -T "${bin}" 2>/dev/null \
            | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 | sed 's/GLIBC_//')"
        echo "  glibc floor: ${floor:-unknown}"
        if [ "${floor}" != "{{ GLIBC_FLOOR }}" ]; then
            echo "::error::glibc floor is ${floor}, not {{ GLIBC_FLOOR }}." >&2
            echo "  Update GLIBC_FLOOR here *and* the number quoted in" >&2
            echo "  release.yml's notes, README.md and docs/PHASE5.md §10." >&2
            exit 1
        fi ;;
    esac

    rm -rf "${staging}"
    mkdir -p "${stage}"
    cp "${bin}" "${stage}/tf_tree"
    ln -s tf_tree "${stage}/tft"
    cp -L LICENSE-MIT LICENSE-APACHE NOTICE README.md "${stage}/"
    for f in LICENSE-MIT LICENSE-APACHE; do
        bytes=$(wc -c < "${stage}/${f}")
        [ "${bytes}" -ge 1000 ] || { echo "::error::${f} is ${bytes} bytes" >&2; exit 1; }
    done
    # Deterministic packaging: pinned mtime, ownership and `--mode` (`X` grants execute only where set or on directories, so the builder's umask cannot reach the checksum); `gzip -n`.
    pack () {
        tar --sort=name --format=gnu \
            --owner=0 --group=0 --numeric-owner \
            --mode='u=rwX,go=rX' \
            --mtime="@$(git log -1 --format=%ct)" \
            -C "${staging}" -cf - "${name}" \
            | gzip -n -9 > "$1"
    }
    pack "${staging}/${name}.tar.gz"

    # Two checks; packing twice and comparing is vacuous (same second, same timestamps).
    stamp="$(od -An -tu4 -j4 -N4 < "${staging}/${name}.tar.gz" | tr -d ' ')"
    if [ "${stamp}" != "0" ]; then
        echo "::error::gzip header carries MTIME ${stamp}; -n is not in effect" >&2
        exit 1
    fi
    find "${stage}" -exec touch -h -d '2001-09-09T01:46:40Z' {} +
    chmod -R g+w "${stage}"
    pack "${staging}/${name}.repack"
    a="$(sha256sum < "${staging}/${name}.tar.gz")"
    b="$(sha256sum < "${staging}/${name}.repack")"
    rm -f "${staging}/${name}.repack"
    if [ "${a}" != "${b}" ]; then
        echo "::error::packaging is not deterministic: staged mtimes or modes reached the archive" >&2
        exit 1
    fi
    owners="$(tar tvzf "${staging}/${name}.tar.gz" | awk '{print $2}' | sort -u)"
    if [ "${owners}" != "0/0" ]; then
        echo "::error::archive records ownership '${owners}', expected 0/0" >&2
        exit 1
    fi

    check="${staging}/roundtrip"
    rm -rf "${check}" && mkdir -p "${check}"
    tar xzf "${staging}/${name}.tar.gz" -C "${check}"
    [ -L "${check}/${name}/tft" ] || { echo "::error::tft is not a symlink in the archive" >&2; exit 1; }
    unpacked="$("${check}/${name}/tft" --version)"
    [ "${unpacked}" = "${want}" ] || { echo "::error::unpacked tft reports '${unpacked}'" >&2; exit 1; }
    rm -rf "${check}"

    ( cd "${staging}" && sha256sum "${name}.tar.gz" > "${name}.tar.gz.sha256" )
    echo "  packaged: ${staging}/${name}.tar.gz"
    cat "${staging}/${name}.tar.gz.sha256"

# The CycloneDX SBOM `docs/PHASE5.md` §10 asks for, from `cargo metadata` over `normal` edges from the shipped roots (see `scripts/sbom.py`).
# `lint` depends on it so the generator runs before a tag. `VERSION` is a version, never a tag (`release.yml` passes `${tag#v}`); `OUT` respects `CARGO_TARGET_DIR`.
sbom VERSION=`cargo pkgid -p tf_tree_cli | sed 's/.*[@#]//'` OUT="":
    #!/usr/bin/env bash
    set -euo pipefail
    out="{{ OUT }}"
    if [ -z "${out}" ]; then
      out="${CARGO_TARGET_DIR:-target}/tf_tree-{{ VERSION }}-sbom.cdx.json"
    fi
    mkdir -p "$(dirname "${out}")"
    python3 scripts/sbom.py --version "{{ VERSION }}" -o "${out}"

# `docs/PHASE2.md` §11.2 scenario 9, a thousand times (§15's box 6): the split-brain race, whose value is the tail. The child's open timeout is shortened so the ownerless arm does not sleep 83 minutes.
split-brain-soak RUNS="1000":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --quiet -p tf_tree --features shm,unstable --tests
    echo "running §11.2 scenario 9 x {{ RUNS }}"
    for i in $(seq 1 {{ RUNS }}); do
        if ! cargo nextest run -p tf_tree --features shm,unstable \
             --test rendezvous -E 'test(/scenario_9_/)' >/tmp/tf-split-brain.log 2>&1; then
            echo "::error::split-brain FAILED on run ${i} of {{ RUNS }}" >&2
            cat /tmp/tf-split-brain.log >&2
            exit 1
        fi
        if [ $((i % 50)) -eq 0 ]; then echo "  ${i}/{{ RUNS }} clean"; fi
    done
    echo "§11.2 scenario 9: {{ RUNS }} consecutive runs, no second instance_uuid"

# `docs/PHASE2.md` §12.3 gate 4: kill -> re-claimable p99 under 10 ms, as a supervisor sees it. INVALID, not FAIL, when the edge was takeable on the first attempt (that measured teardown).
reclaim-latency TRIALS="200":
    cargo build --release --features shm -p tf_tree_bench --bin reclaim_latency
    TRIALS={{ TRIALS }} ./target/release/reclaim_latency
