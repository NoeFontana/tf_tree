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

# **The `compile_fail` error-code pins, which stable rustdoc does not check.**
#
# Two doc tests assert that a type is *not* `Sync` — `tf_tree::OwnedWriter` and
# `tf_tree_core::edge::Publisher` (`docs/PROJECT.md` §5 D7). A bare
# `compile_fail` passes when the snippet fails to compile for **any** reason,
# including the type having been renamed, moved or un-exported, so both are
# written `compile_fail,E0277` to pin the unsatisfied-trait-bound failure that is
# actually under test.
#
# **Stable rustdoc parses the code and then ignores it.** Measured, by mutating
# both pins to `E0599`: `cargo test --doc -p tf_tree -p tf_tree_core` still
# reports `ok` on stable, and `cargo +nightly test --doc` fails with *"Some
# expected error codes were not found: \["E0599"\]"*. So `just test-doc` — the
# `--workspace` line above, on stable — is not the gate for these, and without
# this recipe nothing was: the nightly toolchain appears elsewhere only in
# `just miri` and `just c-abi-check`, neither of which runs a doctest.
#
# Kept separate from `test-doc` rather than folded into it because it is the one
# doctest command that requires nightly; a contributor without that toolchain
# gets a clear failure from a named recipe instead of a mystery from the main
# test gate. It is seconds long, and CI runs it on the nightly job.
test-doc-error-codes:
    cargo +nightly test --doc -p tf_tree -p tf_tree_core

# Concurrency model checking under loom (reduced buffer capacities).
loom:
    cargo xtask loom

# Undefined-behavior checking under Miri (arena + core + the facade).
#
# `miri-soft-float` is opt-in here and nowhere else: Miri cannot execute the x86
# inline `sqrt` asm libm's default `arch` feature emits. Enabling it globally
# would compile the shipped binaries and the benchmarks with soft floats too.
#
# **`tf_tree` is here because `docs/decisions/0017` put a lifetime extension in
# it** (`OwnedWriter` — the facade's only `unsafe`, and now the **workspace's**
# only one: that record's steps 6–7 have deleted `tf_tree_c`'s and
# `tf_tree_py`'s `extend_to_static` helpers, so this recipe covers every
# lifetime extension there is rather than one of three). That record names
# `just miri` as the verification for its step 2 — a gate it could not perform while
# the crate was excluded. It earned its place immediately: adding it caught a
# real *"deallocating while item [SharedReadOnly …] is strongly protected"* in
# the first version of that type, which no other gate in this repository could
# see. See also the `c-abi-check` comment below on not leaving crates out.
#
# It gets its own line because it needs `-Zmiri-disable-isolation`: building a
# `Tree` reads `/proc/sys/kernel/random/boot_id` and the process start time
# (A7's reboot check), and Miri refuses filesystem access without it. Default
# features only, deliberately — `shm` is `memfd_create` and `fcntl(F_OFD_*)`,
# which Miri cannot execute at all (`0005` *What we commit to*), and the `fork`
# half of the same protocol lives in `tf_tree_bench`'s `fork_child` binary.
# Both are covered by `just shm-check` instead, which builds that binary and
# runs the `fork` and `multiprocess` suites.
#
# **Two targets, not the whole crate, and this is a measurement rather than a
# preference.** The facade's `unsafe` is one block, and `tests/owned_writer.rs`
# is what exercises it; the crate's other targets are safe-code numerics whose
# arena and engine work is already interpreted by the command above, against the
# crates that own it. Under Miri they cost hours for no additional UB coverage:
# `tests/batch.rs` had not finished after twenty minutes, and
# `tests/construction.rs` — the cheapest of them — takes 160 s against this
# pair's 2 s. **If a second `unsafe` ever lands in `tf_tree`, the target that
# covers it joins this line**; that is the whole rule, and a `--test` list is
# how it stays visible instead of silently drifting to nothing.
miri:
    cargo +nightly miri test -p tf_tree_arena -p tf_tree_core \
        --features tf_tree_core/miri-soft-float
    MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p tf_tree \
        --features tf_tree_core/miri-soft-float --lib --test owned_writer

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
# **`tf_tree_ingest`'s feature axes, which `--workspace` compiles exactly one of.**
#
# `fixture` is off by default and reachable only through the crate's
# self-referential dev-dependency, so `cargo clippy --workspace --all-targets`
# compiles its module to nothing. That is the same shape as the defects
# `test-rust` and `shm-check` already carry comments about, one crate over.
#
# This recipe exists now rather than when the codecs land, because the codec-free
# build is about to become the *non-default* configuration — the one
# `IngestError::CompressedChunk` exists for, and the one nothing would compile.
ingest-check:
    cargo clippy -p tf_tree_ingest --features fixture --all-targets -- -D warnings
    cargo nextest run -p tf_tree_ingest --features fixture
    # **The codec-free build, which `--workspace` compiles nowhere.**
    #
    # `tf_tree_ingest`'s `compression` feature is default-**on**, so every other
    # recipe in this file compiles exactly one configuration and
    # `#[cfg(not(feature = "compression"))]` code — `tests/codec_free.rs`, the
    # `is_built_in` arm that refuses a codec, `fixture`'s refusal to write one —
    # is compiled by nothing. That is the same shape as the four defects
    # `test-rust` and `shm-check` exist for, one crate over: a configuration
    # nobody builds is not a checked configuration, and here the unchecked one is
    # what a `--no-default-features` consumer gets.
    #
    # Verified to be a real gate rather than a no-op: this line runs 87 tests,
    # five of which exist only in this configuration.
    cargo clippy -p tf_tree_ingest --no-default-features --all-targets -- -D warnings
    cargo nextest run -p tf_tree_ingest --no-default-features
    # The CLI's `ingest_err` arms are the only place the remedy text for a
    # compressed or bad chunk exists, and they are reachable only from a build
    # that can produce those errors.
    cargo clippy -p tf_tree_cli --all-targets -- -D warnings
    cargo nextest run -p tf_tree_cli
    # And the CLI without its defaults, which drops both `counters` and
    # `compression` — the second forwards to `tf_tree_ingest/compression`, and the
    # workspace declares that dependency `default-features = false`, so this is
    # the configuration where a missing feature edge would show up as a CLI that
    # cannot read an ordinary bag.
    cargo clippy -p tf_tree_cli --no-default-features --all-targets -- -D warnings
    cargo nextest run -p tf_tree_cli --no-default-features
    # **The shipped CLI links both codecs, asserted against the dependency graph
    # because no test inside the crate can assert it.**
    #
    # `tf_tree_cli/tests/ingest.rs::the_cli_compression_feature_switches_the_reader`
    # catches a feature edge that is *wired wrong* — a dependency re-enabling or
    # failing to forward `tf_tree_ingest/compression` independently of what the CLI
    # asked for, the way `tf_tree_bench` once did to `counters`. It cannot catch
    # `compression` being **deleted** from `[features] default`, and this was
    # verified rather than assumed: with the feature removed from the manifest,
    # `cargo nextest run -p tf_tree_cli` ran 117 tests and all 117 passed. Every
    # `cfg!` in the crate is relative to the configuration being deleted, so both
    # sides of that assertion go `false` together and the end-to-end zstd test
    # compiles to nothing.
    #
    # The workspace declares `tf_tree_ingest` with `default-features = false`, so
    # the deletion ships a `cargo install tf_tree_cli` that refuses every zstd bag —
    # the ordinary rosbag2/Foxglove case — while `cargo build --workspace`,
    # `just lint` and `cargo nextest run --workspace` all stay green through feature
    # unification. The graph is the only place the invariant is visible.
    cargo tree -q -p tf_tree_cli -e normal | grep -q ruzstd || \
        { echo "tf_tree_cli's default build has no zstd decoder: is 'compression' still in [features] default?"; exit 1; }
    cargo tree -q -p tf_tree_cli -e normal | grep -q lz4_flex || \
        { echo "tf_tree_cli's default build has no lz4 decoder: is 'compression' still in [features] default?"; exit 1; }

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
    # `tf_tree_ingest`'s `fixture` module is default-off and so is invisible to the
    # workspace pass above, for the same reason. See `ingest-check`.
    cargo clippy -p tf_tree_ingest --features fixture --all-targets -- -D warnings
    # **And its `compression` axis, which is the *other* direction: default-ON, so
    # the workspace pass compiles the codec-free half nowhere.** This line belongs in
    # `lint` and not only in `ingest-check`, because `lint` is the recipe CI's lint
    # job mirrors and the recipe a contributor runs before pushing: without it, a
    # clippy error under `--no-default-features` — `tests/codec_free.rs`, the
    # `is_built_in` arm, `fixture`'s `CodecUnavailable` arm — was reachable only by
    # someone who also ran `just test`.
    #
    # Verified to be a real gate: a `clippy::len_zero` injected into
    # `tests/codec_free.rs` left `cargo clippy --workspace --all-targets -- -D warnings`
    # finishing clean, and this line failed on it.
    cargo clippy -p tf_tree_ingest --no-default-features --all-targets -- -D warnings
    cargo clippy -p tf_tree_cli --no-default-features --all-targets -- -D warnings

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

# **The MSRV floor, on the host rather than only in CI.**
#
# `SUPPORT.md` calls the floor "enforced, not intended", and until this recipe
# existed the only thing enforcing it was CI's `msrv` job — which has produced no
# run since 2026-07-23. A floor whose only gate is a workflow nobody is running is
# back to being intended, which is the exact failure that took `rust-version` from
# 1.83 to 1.85: the number looked authoritative and nothing had ever compiled
# against it.
#
# This mirrors that job step for step, and deliberately so — the version is read
# out of the manifest rather than written here, the `--locked` build uses the
# committed lockfile (a transitive crate that quietly needs a newer toolchain is
# the drift being caught, so re-resolving would hide it), and `--lib --bins`
# because the promise covers what a downstream *links*, not what our
# dev-dependencies need.
#
# Requires the floor's toolchain to be installed; when it is not, the recipe stops
# with the exact `rustup toolchain install` line to run rather than falling back to
# `stable`, which would make it pass while checking nothing.
msrv:
    #!/usr/bin/env bash
    set -euo pipefail
    want=$(grep -m1 '^rust-version' Cargo.toml | cut -d'"' -f2)
    test -n "$want" || { echo "no rust-version in Cargo.toml"; exit 1; }
    rustup toolchain list | grep -q "^$want" \
        || { echo "the floor is $want; install it: rustup toolchain install $want"; exit 1; }
    echo "==> building the workspace on the declared floor, $want"
    cargo "+$want" build --workspace --lib --bins --locked
    # `cargo build --workspace` cannot see `crates/tf_tree_py` or
    # `crates/tf_tree_tf2_sys`: both are excluded from the workspace (maturin builds
    # one, the other needs a ROS 2 install), so both spell `rust-version` by hand and
    # neither is compiled by the line above. Compared rather than compiled, which is
    # the strongest check available for a crate this host cannot build.
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
    # **The prose too, because that is where the last drift was.** `README.md`
    # said 1.85 while the manifest said 1.87 — the two arms above both passed,
    # since neither had ever looked at a file a user reads. A floor stated only
    # in a manifest is a floor stated nowhere: `cargo` enforces it, and the
    # person deciding whether they can adopt the crate never opens `Cargo.toml`.
    #
    # Matched loosely (`**1.87**` anywhere in the file) rather than by line, so
    # the sentence can be rewritten without breaking the gate; what may not
    # change silently is the number.
    echo "==> the number is stated where a user reads it, and still agrees"
    for f in README.md SUPPORT.md crates/tf_tree/src/lib.rs; do
        if ! grep -qF "**$want**" "$f"; then
            echo "$f: does not state the MSRV as **$want**"
            rc=1
        fi
    done
    exit $rc

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

# **`docs/PHASE5.md` §9.2's two embedding measurements.** One is gated.
#
# 1. **GATED, and it is §9.2's row.** Inside *one* build, at one profile, two
#    identical `#[inline(never)]` depth-3 lookups are timed: one compiled in
#    `tf_tree_bench` (an embedder's position), one in `tf_tree_core` (the crate
#    that defines `Plan::at` and the fold). The difference is the crate boundary
#    and nothing else. §9.2 requires the row be reported at an embedder's default
#    profile, so it is read off the `[profile.embedder]` run; the
#    `[profile.release]` run is printed as the control, where `lto = "thin"`
#    erases the boundary at link time.
# 2. **EXPLORATORY, and never gated.** The same out-of-crate column across the
#    two profiles — what `docs/API.md` §2.3 item 2's LTO guidance is worth. Two
#    processes seconds apart, so it carries the host's full between-run noise;
#    `docs/PHASE1.md` §11.2's exploratory shape. It is printed and written to
#    `target/embed-cost/`, and it does not enter `results.json`.
#
# `taskset -c 2`, for `cpp-bench`'s reason: an unpinned run migrates cores and
# swings by far more than the 5% criterion allows. **It requires a CPU 2** — i.e.
# at least three logical CPUs — and fails outright rather than silently
# unpinning if there is none. Every run also reports its own round-to-round band,
# and the gated verdict is `unresolved` — never a pass or a fail — when that band
# straddles the 5% threshold, so a gate whose noise floor exceeds its threshold
# reports `unavailable` instead of passing.
#
# **Build footprint, measured on this host: `--profile embedder` is a third
# target directory beside `debug/` and `release/`, `166 MiB` — `rm -rf
# target/embedder` then a clean `cargo build --profile embedder … --bin
# embed_cost`, then `du -sh target/embedder`. The whole recipe, both builds and
# both runs, took 10 s warm.** That is cheap enough that `bench-check` pays it;
# see its comment.
#
# The output pair is left in `target/embed-cost/`. `bench-check` and
# `bench-baseline-update` depend on this recipe and pass that directory with
# `--embed-cost`; `just bench-report --embed-cost target/embed-cost` reads the
# same pair by hand.
embed-cost:
    #!/usr/bin/env bash
    set -euo pipefail
    out=target/embed-cost
    mkdir -p "$out"
    cargo build -q --profile embedder -p tf_tree_bench --features embed-probe --bin embed_cost
    cargo build -q --release -p tf_tree_bench --features embed-probe --bin embed_cost
    taskset -c 2 ./target/embedder/embed_cost --json "$out/embedder.json"
    taskset -c 2 ./target/release/embed_cost --json "$out/release.json"
    ./target/release/embed_cost --compare "$out"

# **fmt / clippy / tests for the default-off `embed-probe` configuration.**
#
# `cargo nextest run --workspace` builds default features, so
# `tf_tree_core::bench_probe` and everything in `tf_tree_bench::embed` that
# drives it are compiled out of `just test` — exactly like `shm`. This is their
# gate, and a new `embed-probe`-only test target belongs on this list in the
# commit that adds it.
embed-cost-check:
    cargo fmt --check -p tf_tree_core -p tf_tree_bench
    cargo clippy -p tf_tree_core --features bench-probe --all-targets -- -D warnings
    cargo clippy -p tf_tree_bench --features embed-probe --all-targets -- -D warnings
    cargo nextest run -p tf_tree_bench --features embed-probe -E 'test(/embed/)'

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
#
# **§9.2's embedding row is measured here, and it must be**, because
# `bench-baseline-update` below measures it too. The baseline gate compares row
# *status* in one direction only: a row that is `measured` in the committed
# baseline and is not one now is a withdrawn claim and a hard failure
# (`src/baseline.rs`). So a baseline cut with `--embed-cost` and a check run
# without it is a gate that fails on the difference between two recipes, on any
# host where the row resolves — it did not fire on this host only because the
# fitness probe fails here and both sides came out `unavailable`. **The two
# paths take the same flag; do not make one of them cheaper.**
#
# The cost is the `embed-cost` recipe's: a 166 MiB `target/embedder` tree and
# 10 s, both measured — next to the minutes this suite already spends assembling
# the report. `bench_report` still runs without the flag (`just bench-report`);
# the row then says so and names `just embed-cost` rather than disappearing.
bench-check: embed-cost
    cargo run --release -p tf_tree_bench --bin bench_report -- \
        --out target/bench-report \
        --embed-cost target/embed-cost \
        --check-baseline crates/tf_tree_bench/baseline/results.json

# Regenerate the committed baseline. **Run this deliberately, and put the diff
# in the same commit as the change that causes it** — the diff is the record of
# what moved and it is the only place a reviewer sees it.
#
# It depends on `embed-cost` for the same reason `bench-check` does, and the two
# must keep agreeing: whatever the check can produce, the baseline must record,
# or the status comparison fails on the recipe rather than on the code.
#
# `index.html` is not committed: it is a rendering of `results.json` and a second
# copy that can disagree with the first.
bench-baseline-update: embed-cost
    cargo run --release -p tf_tree_bench --bin bench_report -- --out target/bench-report \
        --embed-cost target/embed-cost
    cp target/bench-report/results.json crates/tf_tree_bench/baseline/results.json

# --- The performance suite (exploratory; NOT the `bench-check` gate) ---------
#
# These harnesses answer the two questions `bench-report` does not. `bench-report`
# produces `docs/PHASE5.md` §9's artifact and `bench-check` gates it against a
# committed baseline; both are deliberately narrow, because a gate has to be.
# What was missing is everything either side of that: the §11.2 row nobody had
# measured, the axes nobody had swept, the hours nobody had run, and a way to
# ask "did that change help?" in one command.
#
# **None of these feed `just bench-check`.** This host fails `Fitness::probe`
# (four physical cores, SMT on, an unreadable governor), so every timing row here
# would be Indicative, and a gate that flaps is a gate people learn to ignore.
# The one exception is the zero-allocation gate, which is host-independent and
# runs in `just test` where it belongs.

# List the workload catalogue: what each named load is and why it is there.
workloads:
    cargo run --release -p tf_tree_bench --features shm --bin contended_scaling -- --list

# **`docs/PHASE1.md` §11.2's read-scaling row, with the writers and the pinning.**
#
# Every other reader benchmark in this repository runs against a QUIESCENT tree —
# `benches/read_scaling.rs` says so in its own header and `docs/benchmarks/tf2.md`
# lists both gaps under "What is still not measured". This runs N reader processes
# and M writer processes on one shared arena, each `taskset`-pinned to its own
# core, and reports aggregate throughput, per-lookup service percentiles and the
# open-loop cycle tail.
#
# REFUSES TO RUN on a busy machine, for `mp-bench`'s reason: latency here is
# largely a measurement of the scheduler.
#
# PHASE1 §11.2's read-scaling row: N readers x M writers, pinned, on one arena.
contended-scaling *ARGS:
    cargo build --release --features shm -p tf_tree_bench --bins
    taskset -c 0-7 ./target/release/contended_scaling {{ARGS}}

# Where tf_tree bends and where it breaks: lookup cost against tree WIDTH at a
# fixed dynamic-step count, plan-compile and build cost against tree size, ring
# depth from 8 to 1M slots, publish fan-out to 256 edges, and the arena's own
# limits printed by the engine rather than copied from a header.
#
# Needs no ROS and no shared memory — every axis is a single-process property.
#
# Extreme-scale sweep: width, depth, ring size, publish fan-out, and the limits.
scale-sweep *ARGS:
    cargo run --release -p tf_tree_bench --bin scale_sweep -- {{ARGS}}

# Steady state over minutes: does the tail drift, does RSS grow, do the rings
# actually lap? EXITS NON-ZERO if the last interval's p99.9 exceeds the first's
# by more than 3x, if RSS grows past 8 MiB, or if the rings never lapped — the
# last of which is a failure of the experiment rather than of the engine, and is
# the vacuous-green case `docs/PHASE2.md` §11.4's torture harness was rewritten
# to avoid.
#
# Long-duration steady state: does the tail drift, does RSS grow, do rings lap?
soak *ARGS:
    cargo run --release -p tf_tree_bench --bin soak -- {{ARGS}}

# The overnight version. Thirty minutes laps the fixture's 10 s rings about 180
# times, which is the only way the wraparound path is exercised at all.
#
# The overnight soak: 30 minutes on fleet_16, one snapshot a minute.
soak-long:
    cargo run --release -p tf_tree_bench --bin soak -- \
        --workload fleet_16 --duration 30m --interval 60s \
        --json target/bench-runs/soak-long.json

# --- The A/B loop: did that change help? ------------------------------------
#
# Every harness above takes `--json <path>`. `bench-run` writes one file per
# commit, `bench-ab` compares two and exits non-zero on a regression, so it drops
# into a bisect script without further wrapping.
#
# The direction a metric may move and the slack below which a move is not news
# both travel IN the file, next to the number. Nothing in the differ infers
# either from a key name — that is `results.json` schema /2's argument, one level
# down: a checker that guesses will one day pass a doubled latency because
# somebody named a field `ops_ns`.

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

# Compare two run files. Non-zero exit means something regressed past its own
# tolerance.
#
# Compare two run files; non-zero exit means a metric regressed past its tolerance.
bench-ab a b:
    cargo run --release -p tf_tree_bench --bin bench_ab -- {{a}} {{b}}

# --- Profiling: where does the time actually go? ----------------------------

# Sampling profile of a workload, folded to a flamegraph.
#
# Uses the `profiling` profile — release codegen with debuginfo kept — because
# `[profile.release]` strips it and every tool then falls back to function-level
# attribution, at which point the answer is "it is all in fold_at", which is true
# and tells you nothing. `profile-lookup` relies on the same thing.
#
# `perf` needs `kernel.perf_event_paranoid <= 1`; this host ships 4. The recipe
# checks and prints the one command that fixes it rather than failing obscurely,
# and points at the simulated path below, which needs no permissions at all.
#
# Sampling profile of a workload, for a flamegraph. Needs perf_event_paranoid <= 1.
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

# Exact, simulated, and needs no privileges: instruction counts and cache misses
# per source line over any workload.
#
# The generalisation of `profile-lookup`, which is pinned to `footprint`'s one
# hardcoded query. Simulated, so no idle machine is needed and the counts are
# exact — but it is roughly 50x slower than native, so keep the workload small.
#
# Per-line instruction counts and cache misses over a workload. No privileges needed.
profile-cachegrind workload="robot":
    #!/usr/bin/env bash
    set -euo pipefail
    command -v valgrind >/dev/null || { echo "valgrind is not installed" >&2; exit 1; }
    cargo build --profile profiling -q -p tf_tree_bench --bin soak
    mkdir -p target/profile
    valgrind --tool=cachegrind --branch-sim=yes --cache-sim=yes \
        --cachegrind-out-file=target/profile/cg.out \
        ./target/profiling/soak --workload {{workload}} --duration 4s --interval 2s \
        >/dev/null 2>&1 || true
    cg_annotate --show=Ir,Bcm,D1mr --sort=Ir --auto=yes target/profile/cg.out

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

# **`docs/PHASE5.md` §9.1's end-to-end comparison, over a real DDS.**
#
# The one measurement in this repository that includes the transport. Every other
# tf2 comparison here feeds `tf2::BufferCore` in-process, which is deliberately
# generous to tf2 and is not what a deployed node pays — `mp_bench` says so in
# its own output ("this tf2 column is a FLOOR ... but no transport"). This runs N
# `tf2_ros::TransformListener` consumers against one publisher over the container's
# real RMW, and the same query set through the ingest bridge.
#
# **It prints, every run, the arm it cannot measure and why**: the bridge builds a
# heap arena, so there is no multi-process tf_tree arm until a decision record
# gives it a shared one. §9.3 is normative that an honest gap beats a favourable
# number nobody trusts.
#
# Env: WORKLOAD, CONSUMERS, SECONDS_MEASURED, WARMUP, HZ.
#
# N tf2 listeners over DDS against the bridge, on identical data and QoS.
dds-bench *ENV:
    ./docker/tf2/run.sh './ros/build.sh && {{ENV}} ./ros/dds_bench.sh'

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
# `TF2_WRITERS=N` adds N writer threads per engine, on dynamic edges the query
# path does NOT traverse — PHASE1 §11.2's contended configuration, and the row
# where tf2's single buffer mutex and tf_tree's per-edge seqlock differ most.
# Default 0, so the quiescent rows stay the continuity anchor.
#
# Concurrent read scaling, both engines interleaved. TF2_WRITERS=N to contend it.
tf2-scaling *ENV:
    ./docker/tf2/run.sh '{{ENV}} cargo run -p tf_tree_bench --features tf2 --release --bin tf2_scaling'

# Instructions per `Ingest::offer` — cachegrind, N=0 baseline subtracted.
#
# The bridge's answer to `footprint`, and it exists for the same reason: this
# host fails `tf_tree_bench`'s `Fitness::probe` (four physical cores, SMT on, an
# unreadable governor) and `perf_event_paranoid` is 4, so hardware counters are
# denied. cachegrind simulates, so unlike `bridge-cost` below this does NOT need
# an idle machine — every number is exact under load, and slow for the same
# reason (~50x).
#
# **The sweep is the measurement, not decoration.** A `BTreeMap` with one key
# compares nothing, so `edges=1` is the control: if `Ir`/offer does not rise from
# 1 to 100 edges, there is no lookup cost to remove and any refactor claiming
# otherwise is unjustified. Name style is swept for the same reason — `link0`
# shares four bytes with `link1`, a real `robot1/arm/wrist_0_link` shares fifteen.
#
# Cache geometry is pinned rather than read from CPUID, so a rebuilt image cannot
# silently move `D1 misses` while `Ir` stays put.
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

# Wall-clock cost of one `tft_bridge_offer`, through the C ABI.
#
# **RUN THIS ON AN IDLE MACHINE, and it is still not a claim.** `bridge_cost.rs`
# has had no recipe since it was written; this is it. The host fails the timing
# fitness probe, so the output is indicative and belongs in a commit message
# marked as such, never in `results.json` as `measured`.
bridge-cost:
    cargo build --release -q -p tf_tree_c --features bridge --example bridge_cost
    taskset -c 2 ./target/release/examples/bridge_cost

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
    # **`docs/decisions/0017` steps 2 and 3 — and this line is the rule three
    # paragraphs above being obeyed rather than restated.** Half of
    # `tests/owned_writer.rs` is `#[cfg(all(feature = "shm", target_os =
    # "linux"))]`, because a claim *lease* is an OFD byte in the rendezvous lock
    # file and a heap tree has no lock file at all. Those two tests are the ones
    # that reproduce the shipped `tf_tree_py` defect — a leaked lease, so the
    # edge is permanently unclaimable and invisible to every reaper — and
    # `--workspace` compiles them out. The other half runs under `just test`.
    cargo nextest run -p tf_tree --features shm --test owned_writer
    # **`docs/PHASE2.md` §11.4's torture harness, gated on a branch.** The
    # nightly is `just shm-torture` (30 minutes); this is the seconds-long
    # self-test that proves the harness's detector still detects. A soak test
    # whose checker has silently stopped checking prints exactly what a passing
    # one does, so this is the difference between a gate and a decoration.
    # Kept as the command rather than `just shm-torture-self-test` so this
    # recipe stays a flat list somebody can read top to bottom; the standalone
    # recipe exists for running it alone.
    cargo nextest run -p tf_tree_bench --features shm --release --test torture
    # **`docs/PHASE5.md` §3 into §2's container.** `tests/frozen_bag.rs` carries
    # `required-features = ["shm"]`, so `cargo nextest run --workspace` — which
    # builds without features — skips the target entirely. The rest of
    # `tf_tree_ingest`'s suite does run there; this is the half that cannot.
    cargo clippy -p tf_tree_ingest --features shm --all-targets -- -D warnings
    cargo nextest run -p tf_tree_ingest --features shm
    cargo nextest run -p tf_tree_ipc

# **`docs/PHASE2.md` §11.4's `shm_torture`, which `docs/PHASE5.md` §10 wants
# running nightly.** N processes on one arena doing random
# attach/detach/claim/reap/push/lookup while the driver `SIGKILL`s one of them
# several times a second and replaces it. Every reader validates every transform;
# after the run a surviving participant checks that no claim and no participant
# slot was leaked by a process that never got to clean up.
#
# **Thirty minutes and six children is `docs/PHASE2.md` §13's spelling**, and it
# is what the nightly job runs. Override for a local smoke run:
# `just shm-torture "--duration 60s --children 4"`.
#
# `--release`: the point is to interleave real processes at real speed. A debug
# build spends its time in bounds checks and reaches a different, much narrower
# set of interleavings.
#
# **What this does NOT cover** is §11.3's crash-point injection — there is no
# `crash-points` feature to arm (§0.0 records it as not implemented), and the
# binary *refuses* `--crash-points` rather than running the SIGKILL test and
# calling it §11.3 coverage. §11.4's "under ASan" half is `just shm-torture-asan`.
shm-torture *ARGS="--duration 30m --children 6 --kill-hz 6":
    cargo build --release --features shm -p tf_tree_bench --bin shm_torture
    ./target/release/shm_torture {{ARGS}}

# **The torture harness's own gate**, seconds rather than minutes, so it belongs
# on a branch rather than in a nightly.
#
# It runs the binary twice and asserts the two runs disagree: once with a child
# publishing a deliberately corrupt transform (which some *other* process must
# catch) and once clean (which must pass having validated thousands of reads).
# Without it, a harness that quietly stopped reading would print the same
# "0 violations" forever — which is exactly what the first revision of this
# harness did, and how the writer pacing in `work` came to exist.
shm-torture-self-test:
    cargo nextest run -p tf_tree_bench --features shm --release --test torture

# **§11.4's "run it under ASan"**, on a short run.
#
# ASan works across `fork`/`exec`, so the children are instrumented too — which
# is the whole reason to run a *multi-process* soak under it. Miri cannot reach
# any of this: there is no `memfd`, no `mmap` and no second process under Miri,
# so the arena's raw-memory `unsafe` has no other dynamic checker at all once it
# crosses a process boundary.
#
# `-Zbuild-std` because the interceptors need an instrumented std; without it
# ASan sees our allocations but not std's, and reports on its own internals.
# It is also why this is minutes and not seconds, and why the duration is short.
#
# `detect_leaks=0`: a `SIGKILL`ed child is *defined* to leak — it is killed
# holding a mapping and an allocation — so LeakSanitizer would report the thing
# the test is deliberately doing. ASan's memory-error detection, which is what
# this is for, is unaffected.
#
# The target is the **host's**, read from `rustc -vV`, not a hardcoded
# `x86_64-unknown-linux-gnu`. `-Zsanitizer` needs an explicit `--target` (an
# implicit one would build the proc-macros and build scripts instrumented too),
# and spelling one architecture there made the recipe silently x86-only — on the
# aarch64 job it would build a cross target that is not installed and fail for a
# reason that says nothing about the arena. `baseline.rs`'s `PORTABLE_FACTS`
# reasons explicitly about aarch64 running these gates.
shm-torture-asan *ARGS="--duration 120s --children 4 --kill-hz 4":
    RUSTFLAGS="-Zsanitizer=address" ASAN_OPTIONS=detect_leaks=0 \
    cargo +nightly run -Zbuild-std --target "$(rustc -vV | sed -n 's/^host: //p')" \
        --release --features shm -p tf_tree_bench --bin shm_torture -- {{ARGS}}

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
