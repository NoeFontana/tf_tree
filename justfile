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
    # none of it.** `crates/tf_tree_c/src/bridge.rs` — **ten** `extern "C"` entry
    # points, the only ones that both decide and write — could be replaced with
    # literal garbage and `cargo build --workspace --all-targets`, this
    # `nextest` line above and `cargo clippy --workspace --all-targets` all
    # still passed; only `cargo fmt --check` noticed. Its tests ran nowhere
    # but the two `+nightly` rows of `just c-abi-check`, so on a machine without
    # a nightly toolchain the §5 seam had no gate at all.
    #
    # **Counts, re-measured** — this comment said "nine entry points" and
    # "21 tests" and both were stale. `grep -c 'extern "C" fn' bridge.rs` is 10;
    # `cargo nextest run -p tf_tree_c --features bridge` runs **63** tests
    # against **31** without the feature, so the feature is worth **32** tests
    # that `cargo nextest run --workspace` never runs.
    #
    # This is the same shape `shm-check` exists for, one crate over: a
    # default-off feature is invisible to `--workspace`, and a file nobody
    # compiles is not a checked file.
    #
    # **This line is `bridge`-without-`shm`, and since `docs/decisions/0015`
    # that is a shipped configuration rather than only a build artifact**: it
    # carries `tft_bridge_options::arena_name` with no `tf_tree::Open` behind
    # it, and `a_shared_arena_without_the_shm_feature_is_refused` is
    # `#[cfg]`-ed to exist *only* here. `bridge,shm` is `just shm-check`'s, for
    # the ordinary reason `shm` is Linux-only and this recipe runs on the
    # aarch64 matrix.
    cargo nextest run -p tf_tree_c --features bridge
    # **`crash-points` (`docs/PHASE2.md` §11.3) — eight tests `--workspace` does
    # not run.** Same shape as the `bridge` row above and `just shm-check`: a
    # default-off feature is invisible to `--workspace`, and a test nobody runs
    # is not a test. These re-execute the test binary as a child, arm one named
    # site through `TF_TREE_CRASH_AT`, and assert the child died of `SIGABRT` at
    # that site and that the state it left is repairable — so they are the only
    # thing standing between a mis-placed `crash_point!` and a fault-injection
    # harness that proves nothing.
    cargo nextest run -p tf_tree_core --features crash-points

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

# **`tf_tree` with `unstable` OFF — the configuration every published consumer
# gets, and the one nothing else in this file compiles.**
#
# `docs/API.md` §2.6's tier split puts `tf_tree::unstable::*` and
# `Tree::arena_view` behind a default-off feature. `cargo build --workspace`
# cannot reach the tier that split is *for*: `tf_tree_cli`, `tf_tree_c`,
# `tf_tree_bench` and `tf_tree_py` each declare
# `tf_tree = { features = ["unstable"] }`, the resolver unifies features across a
# workspace build, so `--workspace` compiles the facade **with** the feature,
# always. `-p tf_tree` is the only package selection that does not, which is why
# every line below is `-p`. (Four, not three. The scan at the bottom of this
# recipe compares `docs/API.md` §6 row 4 and the manifest's `unstable` comment
# against the manifests — it does **not** read this header, which is how this
# sentence shipped naming three: `crates/tf_tree_py/Cargo.toml` already carried
# `features = ["shm", "unstable"]` on the day it was written.)
#
# **Stated precisely, because the loose version is wrong — and the loose version
# used to be here.** It claimed no other recipe compiles `tf_tree`'s own default
# feature set or its `shm` variant. Both halves are false, measured with
# `cargo tree -e features -f '{p} FEATURES={f}'`:
#
#     -p tf_tree                                      FEATURES=counters,default
#     -p tf_tree --features tf_tree_core/miri-soft-float  FEATURES=counters,default
#     -p tf_tree --features shm                       FEATURES=counters,default,shm
#
# so `just miri` (`-p tf_tree --features tf_tree_core/miri-soft-float --lib
# --test owned_writer`) and `just test-doc-error-codes` (`--doc -p tf_tree -p
# tf_tree_core`) both reach the default set, and `just shm-check`, `just tsan`
# and `just shm-rendezvous` all reach the `shm` variant. `just ingest-check`
# reaches a third configuration, no features at all, because
# `[workspace.dependencies]` declares `tf_tree = { default-features = false }`.
#
# What is genuinely unique to this recipe is narrower and worth keeping for its
# own sake: the **whole set of stable-tier checks in one place** — clippy on
# three configurations, the tier's own rustdoc (`just doc` renders the facade at
# `--all-features`, where a link into `tf_tree::unstable` resolves and here it
# does not), the consumer-list scan, and the test-count floor. `docs/API.md`
# §2.6 states the corrected version; do not re-derive it here.
#
# **Verified to be a real gate, by breaking it.** Reverting the branch's own
# `frozen.rs` fix — `self.view()` back to `self.arena_view()`, a crate-internal
# call to a method the feature gates — is invisible to
# `cargo check -p tf_tree --all-targets --features shm` and to
# `cargo check --workspace --all-targets`: both print `Finished` and exit 0,
# because a test target pulls the dev-dependency that turns the feature on. The
# `--features shm` line below reports
# `error[E0599]: no method named 'arena_view' found for reference '&Tree'`
# at `crates/tf_tree/src/frozen.rs:239:25`.
#
# **What this covers is the library, not `tf_tree`'s test targets** — every
# clippy line below is `--lib`, deliberately. The rest of that sentence used to
# read that no `tf_tree` test target could *ever* be compiled with the feature
# off, because the manifest dev-depended on the crate itself with
# `features = ["unstable"]` and unification made the choice for every recipe.
# 0.0.1 deleted that line (the manifest's `[dev-dependencies]` comment records
# why: it does not survive `cargo package`), so the constraint is gone —
# `cargo nextest list -p tf_tree --lib --tests` reports **70** tests where
# `--features unstable` reports **77**. The seven are `tests/counters.rs` whole
# plus one test each in `construction.rs` and `behavior.rs`: they carry
# `#[cfg(feature = "unstable")]` at the call site now, so on the stable tier they
# do not exist rather than failing to compile.
#
# That does not make this recipe cover them — `--lib` still means `--lib` — but
# it does mean the honest claim is now "this recipe chose not to", not "nothing
# can". The last two lines stay the runtime pass over the tier: two workspace
# crates that link the facade and have suites of their own, for the two
# different reasons recorded where they are.
stable-tier-check:
    #!/usr/bin/env bash
    set -euo pipefail
    # The default set (`counters`), which is what `cargo add tf_tree` gives.
    echo "==> the library, default features"
    cargo clippy -p tf_tree --lib -- -D warnings
    # **The one that catches the mutation above**, because `frozen.rs` and
    # `open.rs` — the two modules that were reaching for the public spelling of
    # an internal view — are `#[cfg(all(feature = "shm", target_os = "linux"))]`
    # and are compiled by neither of the other two lines.
    echo "==> the library, default features + shm"
    cargo clippy -p tf_tree --lib --features shm -- -D warnings
    # Not a hypothetical configuration: `[workspace.dependencies]` declares
    # `tf_tree = { ..., default-features = false }`, so `tf_tree_ingest` and
    # `tf_tree_bridge` already link this one.
    echo "==> the library, no default features"
    cargo clippy -p tf_tree --lib --no-default-features -- -D warnings
    # **Rustdoc, on the stable tier alone.** CI's `docs` job builds
    # `--all-features`, where a link into `tf_tree::unstable` resolves; from the
    # tier a published consumer sees, the same link is broken. Nothing else
    # renders these docs the way docs.rs will *not*.
    echo "==> the stable tier's own documentation"
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p tf_tree
    # **The waiver list, checked against the manifests rather than against
    # itself.** `crates/tf_tree/Cargo.toml`'s `unstable` comment and
    # `docs/API.md` §6 row 4 both enumerate who turns the feature on, and they
    # disagreed on this branch — three against four. Neither is the source of
    # truth; the `[dependencies]` entries are, so both are compared to them.
    #
    # **`[dependencies]` and `[dev-dependencies]` are counted separately**, and
    # the awk section tracker is why. They are not the same fact: a shipped
    # dependency is a consumer whose *release* breaks at a patch bump, a
    # dev-dependency only breaks a test run. Lumping them together is what would
    # make the number in the prose four or five depending on which document you
    # read, which is the drift this check exists to stop.
    #
    # **`want_dev` lost `tf_tree` in 0.0.1** — `crates/tf_tree/Cargo.toml` no
    # longer dev-depends on itself, so the scan yields `tf_tree_bridge` alone and
    # this list has to say so or the recipe fails on its own record. Since these
    # are exact-set comparisons, keeping `tf_tree` off `want_dev` is also what
    # makes re-acquiring that line fail here: it would come back as a name the
    # documents do not carry. What that does *not* cover is the other half of the
    # trade the manifest describes — a misspelled `#[cfg(feature = "unstable")]`
    # at a call site silently drops a test and no scan of a manifest can see it.
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
    # **Both documents must name every shipped consumer — in the place that
    # makes the claim, not anywhere in the file.** The first version of this
    # check searched each file whole, and deleting `tf_tree_py` from
    # `docs/API.md` §6 row 4 did not fail it: the name occurs a dozen other times
    # in that document. So each side is narrowed to the passage that is actually
    # asserting a list — §6's row 4, and the `#` comment block directly above
    # `unstable = []`.
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
    # **A runtime pass over the tier, from the only place one is available —
    # and the two crates below are here for two different reasons.**
    #
    # `tf_tree_ingest` links the facade with **no features at all**:
    # `[workspace.dependencies]` declares `tf_tree = { default-features = false }`,
    # so `-p tf_tree_ingest` executes a suite through a facade built without
    # `unstable`. That is the nearest thing in this repository to *running* the
    # stable tier. It is **not** the same feature set as the first line above —
    # `counters` is on there and off here — and `just ingest-check` already
    # compiles this configuration, so what this line adds is the execution, not
    # the compile.
    #
    # `tf_tree_bridge` is here for the opposite reason. Nothing else selects it
    # with `-p`, and this branch broke exactly that: `config.rs`'s arena
    # assertion left `cargo nextest run -p tf_tree_bridge` failing with
    # `error[E0599]: no method named 'arena_view'` while `--workspace` stayed
    # green, because the resolver unified the feature in from `tf_tree_cli`. Its
    # `[dev-dependencies]` now asks for `unstable` in its own name, so this line
    # builds the facade **with** the feature — it is a standalone-build check,
    # not a stable-tier one, and calling it one would be the same kind of false
    # self-description this recipe exists to end.
    echo "==> the downstream suites, run under -p so nothing unifies for them"
    cargo nextest run -p tf_tree_ingest -p tf_tree_bridge
    # **A floor on each tier's test count, because a `cfg` is a silent switch and
    # the two guards above it cannot see the failure this one catches.**
    #
    # `tests/feature_gates.rs` fails to *compile* on a misspelt feature name, and
    # the manifest scan above fails on an undocumented consumer. Neither can see
    # a target that stops being **run** — a gate that still matches, a `--test`
    # line quietly dropped from a recipe, a `required-features` that stops being
    # satisfied. Nothing inside the crate can: only the runner knows what it ran.
    #
    # Measured at 0.0.1: 70 with `unstable` off, 77 with it on, the seven being
    # `tests/counters.rs` whole plus one test each in `construction.rs` and
    # `behavior.rs`. A floor rather than an equality, so adding tests does not
    # need an edit here.
    #
    # `2>/dev/null` also swallows a compile error, which yields 0 and trips the
    # floor. That is the direction you want it to fail in.
    echo "==> the tier still lists the tests it is supposed to"
    have=$(cargo nextest list -p tf_tree 2>/dev/null | wc -l)
    [ "$have" -ge 70 ] || { echo "the stable tier lists $have tests, was 70 at 0.0.1"; exit 1; }
    have=$(cargo nextest list -p tf_tree --features unstable 2>/dev/null | wc -l)
    [ "$have" -ge 77 ] || { echo "the unstable tier lists $have tests, was 77 at 0.0.1"; exit 1; }

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
#
# **`MIRIFLAGS` is appended to, not assigned.** Line 1 always inherited the
# caller's; line 2 used to clobber it, and that one word was why `ci.yml`'s
# `miri` job spelled both commands out by hand instead of running this recipe —
# CI wants `-Zmiri-strict-provenance` on top, and there was no way to ask for it
# without losing `-Zmiri-disable-isolation`. Now `MIRIFLAGS=… just miri` is the
# stricter run and the recipe is still the single spelling of *which* targets
# are interpreted. With the variable unset — every local run — the expansion is
# empty and this is character-for-character the command it was before.

# Miri over the arena, the core, and the facade's one lifetime extension.
miri:
    cargo +nightly miri test -p tf_tree_arena -p tf_tree_core \
        --features tf_tree_core/miri-soft-float
    MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-disable-isolation" cargo +nightly miri test -p tf_tree \
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
# **Every runnable artifact is either gated by a recipe or registered as a probe.**
#
# This exists because `examples/abi_cost.rs` — which *is* PHASE4 §7 gate
# criterion 1 — was executed by no recipe and no workflow for months while
# `docs/PHASE4.md` recorded its number as a PASS and the example itself printed
# FAIL. A document cited a number that nothing re-derived. An audit found the
# same shape in roughly a dozen other places.
#
# The rule is deliberately weak, because a strong one would be wrong: most of
# these artifacts are one-off diagnostic probes and running them in CI would be
# waste. It requires only that an artifact nothing executes be **declared** in
# `docs/benchmarks/EVIDENCE.md` — as a gate (with its recipe) or a probe (with
# what it established). A new artifact a document starts citing, with neither,
# fails here.
#
# It does NOT check that a probe's recorded number is still true. That is what
# makes it a probe. It checks that somebody can find out.
#
# A script rather than an inline recipe, like `cpp-check`: it needs `cargo
# metadata` and multi-line text processing that `just`'s parser mangles.
#
# **Where it runs:** `just lint` depends on it, so it is on every pull request
# through `ci.yml`'s `lint` job and on every tag through `release.yml`'s. It has
# no job of its own on purpose — 0.81 s measured, and a job's own checkout and
# toolchain install cost more than the check does.

# Every runnable artifact is executed by a recipe or registered as a probe.
evidence-audit:
    ./scripts/evidence-audit.sh

# **The 2027 escape hatch, checked today.** `macos-15-intel` is the last x86_64
# macOS image Actions will offer and it goes away in August 2027 (#180); the
# fallback is cross-building x86_64 from the arm64 runner. This recipe is what
# stops that fallback from quietly stopping working in the meantime.
#
# Three targets, `cargo check` only — this is deliberately **not** a build. The
# link step needs an Apple linker driver and the macOS SDK, which no Linux host
# has; measured, a full `cargo build --lib` for `x86_64-apple-darwin` compiles all
# 178 objects and fails at exactly that one step. So `check` is the most this can
# assert from here, and what it asserts is the half that broke: without
# `pure-hash`, blake3's build script shells out to `cc` with `-arch x86_64` and
# dies before pyo3 is reached at all (exit 101, reproduced both ways).
#
# It is also the only thing that compiles `tf_tree_py`'s `pure-hash` feature.
# `bindings-non-linux` uses native runners, so it never needs the feature and
# never exercises it. A feature no job compiles is a feature that rots.
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

# **No tracked file is build output.** 6 ms measured, and it exists because 358
# MiB of cargo fingerprints and rlibs were committed and merged across three
# pull requests without one test, lint or release gate noticing.
#
# `CARGO_TARGET_DIR=target-p` — how you run two cargo invocations without them
# fighting over one lock — writes a *sibling* of `target/`, which `.gitignore`'s
# anchored `/target/` did not match. `git status` stayed clean because the files
# were tracked, and the published crates.io and PyPI artifacts were unaffected
# because both package from a crate root, above which the junk sat. Nothing in
# the pipeline could see it.
#
# It checks build output by *signature*, not by path: `.gitignore` has now been
# patched three times for this same trap, once per spelling, and a fourth
# spelling would slip past all three. See the script for why these four patterns
# and no others.
no-build-output:
    ./scripts/no-build-output.sh

# **No tracked file carries an unresolved merge-conflict marker.**
#
# The fifth of the repository-property gates, and it exists because
# `docs/decisions/README.md` reached `main` with `<<<<<<< Updated upstream` in
# the middle of its status table and three copies of the last three rows, two of
# them stale. A `git rebase` on a dirty worktree autostashed, rebased, reported
# success, and then popped the stash into a conflict — after which `git status`
# was clean, because the markers were inside a file that got staged in the same
# breath.
#
# **All eighteen CI checks passed on it**, `just lint` included.
# `artifact-versions.py` reads that very table every run and counts cells per row
# against the header; a conflict marker is not a table row, and the duplicated
# rows it did see were well-formed. Nothing else in the workspace reads a
# Markdown table for anything but its shape.
#
# Three markers, not four: `=======` alone is half of every conflict and also a
# Markdown setext heading underline, and this repository is more prose than code.
# Every conflict git writes carries the `<<<<<<<`/`>>>>>>>` pair, so dropping the
# ambiguous one costs no coverage. Measured against the whole tracked corpus
# before it was written: the three matched the one corrupted file and nothing
# else.
no-conflict-markers:
    ./scripts/no-conflict-markers.sh

# **What the diagnostic counters cost a guard — `docs/decisions/0022` question 1.**
#
# The 2x2 that question needs: {release, embedder} x {counters on, off}. All four
# matter, and three of them mislead on their own:
#
#   * at `release` (lto = "thin") `Tree::guard` is inlined and the whole cost
#     shrinks — that profile answers a different question;
#   * with a *hoisted* guard the flush amortises to nothing, which is what
#     `counter_cost` already measures and why it finds no contention;
#   * only a **per-call guard on a WRITABLE arena** pays the flush at all, since
#     `Guard::drop` early-returns on `!is_writable()`.
#
# Both arenas here are writable, so this is the dear configuration on purpose.
# **The runtime path, as a node writes it — and the tail a deadline is set
# against.**
#
# `crates/tf_tree/examples/control_loop.rs` is the example that did not exist:
# the README's worked example is an offline dataloader, so a consumer evaluating
# this for a control loop had nothing showing plan-once, hoist-the-guard,
# extrapolate-on-purpose, or `SlotContended` as data rather than as an error.
#
# It reports and does **not** gate. `docs/PHASE1.md` §11.3's latency criteria
# need core-pinned hardware; this runs on whatever host you have, brackets a
# sub-microsecond operation with two clock reads, and is preempted by whatever
# else is running — all three of which inflate the numbers, and all three of
# which it says in its own output. What it is good for is the *shape*: two
# queries under one guard, and the staleness of a composed route being set by its
# slowest edge.
guard-cost:
    #!/usr/bin/env bash
    set -euo pipefail
    for prof in release embedder; do
      for feat in "--features shm" "--no-default-features --features shm"; do
        case "$feat" in *no-default*) c=off ;; *) c=on ;; esac
        cargo build --profile "$prof" -q $feat -p tf_tree_bench --bin arena_backing
        # `--profile release` builds into target/release, not target/profile-release.
        dir=$([ "$prof" = release ] && echo release || echo "$prof")
        echo "--- profile=$prof counters=$c"
        taskset -c 2 "./target/$dir/arena_backing" 2>/dev/null | grep -E "^  (heap|memfd) arena"
      done
    done


# **What the default interpolator buys, as a function of publish rate** —
# `docs/PROJECT.md` §5 D5's owed measurement. Pairs with `just interp-cost`'s
# three regimes: that one prices the policies, this one prices the difference
# between their answers. Reports; gates nothing.
interp-accuracy:
    cargo run --release -q -p tf_tree_bench --example interp_accuracy

control-loop:
    cargo run --release -q -p tf_tree --features shm --example control_loop

# **One arena, two processes** — the capability `README.md` leads with, runnable.
#
# `control-loop` above is the shape of a node's inner *loop*, and its reader is a
# thread on purpose: that example is about latency, and a thread keeps the
# measurement about the fold. This one is about the **seam** — what a publisher
# declares, what a consumer opens, and how each finds the other from nothing but
# a name. One target with an argv switch rather than two, so it stays one recipe.
#
# Reports; gates nothing. It does assert the consumer succeeded, so it fails
# loudly if the seam breaks.
two-processes:
    cargo run --release -q -p tf_tree --features shm --example two_processes

# **Is the C ABI's +101 ns on a shared arena the ABI, or the C++ caller?**
#
# Four candidates for that gap are eliminated by measurement — the memfd mapping
# (<= 9.6 ns), the cross-process read-only attach (-0.2 ns), static-vs-shared
# linkage (~1 ns) and the per-call `Guard` on this arena (+19.3 ns) — leaving
# ~81 ns unattributed. `just abi-cost` does not reproduce it: its full ABI costs
# +2.3 ns over a native arm that also guards per call, on a 3-edge tree.
#
# The remaining variable is the **caller**. This calls `tft_plan_at` from Rust on
# the same arena, in the same process, against the same stamps, so the ABI is the
# only thing that changes between the two arms. Lands near 302 and the cost is
# the ABI on this fixture; lands near 220 and it is the C++ side, and `0022` is
# aimed at the wrong thing.
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

# **PHASE2 §12's attach rows**, which had never been measured.
#
# §12's table asks for "attach time, cold and warm" (p50) and "first access after
# attach, per-edge population on vs off" (p99.9, both). `benches/` had neither,
# and `report.rs`'s `attach_latency` — a required `where_we_are_worse` entry —
# carried no number at all. An honesty section that cannot regress is not doing
# the job.
#
# **The `population off` arm is deliberately absent**: `populate_hot()` is
# unconditional inside `attach_shared_inner`, and manufacturing an `off` arm out
# of some other code path would be worse than saying so. It arrives with `0022`'s
# B2-prime, which is the change that gives the attach path a policy at all.
#
# Pinned, and needs no idle host beyond that: ~100 us against a ~4% run-to-run
# spread is not a measurement this machine struggles with.
attach-bench:
    cargo build --release -q --features shm -p tf_tree_bench --bin attach_bench
    taskset -c 2 ./target/release/attach_bench

# **PHASE5 §12 gate criterion 4: 16 workers sharing one `.tft`, total Pss within
# 1.2x of one worker — the project's central memory claim, which nothing had
# ever run.**
#
# `just bench-report`'s `tft_16_workers_rss` row has always been UNAVAILABLE for
# two reasons, and both dissolved: the report binary is built without `shm` so it
# has no `Tree::open_frozen` to call (still true — hence a separate binary), and
# the core budget refused sixteen consumers on four cores (retired by
# `report.rs`'s `Sensitivity::Memory` axis — Pss is not a timing measurement, and
# sixteen workers mapping one file share exactly the pages they would share on
# sixteen cores).
#
# **Needs no quiet host and no core parity**, unlike everything else in this
# file's benchmark section. It does need ~340 MiB of disk and ~1 GiB of RAM.
#
# The `.tft` has to be large or the gate is arithmetic about process overhead
# rather than about sharing: with p MiB private per worker, the criterion needs
# S >= 74p. The default shape (64 robots x 40 s, 338 MiB) is chosen for that,
# and it is why §12 gate 2 speaks of a "233 MB index".
#
# **The fixture is deleted first, and that is the whole point of the line.**
# `frozen_workers` reuses `--tft` when the file is there, so without the `rm`
# this recipe measures whatever `.tft` the last run left on disk: 0.26 s and no
# freeze, against 1.6 s when it has to build one. Change the freeze path, run
# `just gate4`, and it validates last week's file and prints PASS — which is
# this repository's own recorded failure, "a gate that passed while asserting
# against a file it had not produced", in the one register written to stop it
# (`docs/benchmarks/EVIDENCE.md`). 1.6 s is not a price worth a stale verdict.
#
# On a hosted runner it happens to be moot — `Swatinem/rust-cache` removes loose
# files under `target/<non-profile-dir>/` before saving, so the fixture is never
# cached — but that is a property of a third-party action's cleanup routine, not
# of this gate, and it is worth nothing locally.
#
# To iterate without re-freezing, run the binary directly:
#   ./target/release/frozen_workers --tft target/gate4/workers.tft --workers 1,16
gate4:
    cargo build --release -q --features shm -p tf_tree_bench --bin frozen_workers
    rm -f target/gate4/workers.tft
    ./target/release/frozen_workers --tft target/gate4/workers.tft --workers 1,16

# **The same measurement with a *Python* worker, because gate 4's verdict is a
# function of the worker's language.**
#
# `(S + 16p)/(S + p) <= 1.2` is `S >= 74p`, so criterion 4 is arithmetic about
# `p` — private bytes per worker — as much as about sharing, and `p` is a
# property of the interpreter, its extension modules and the worker's own
# allocations. None of those belong to tf_tree. `docs/PHASE5.md` §12 gate 4's
# amendment records what follows: the Rust worker's `p` is 0.36 MiB and a
# spawned CPython one's is 13.44 MiB, so the criterion wants ~994 MiB of arena
# where the fixture supplies 338, and **gate 4's own file fails gate 4's own
# criterion at 1.785x with a Python worker.**
#
# **This recipe exists because that 1.785x had no recipe.** `just gate4`
# regenerated the 1.024x and nothing regenerated the qualification, which is
# `docs/benchmarks/EVIDENCE.md`'s founding failure — a recorded number nothing
# re-derives — reappearing inside the register itself. §12 gate 4's amendment
# names the obligation directly: any record that gives criterion 4 a second
# worker arm owes a recipe with it.
#
# **It reports; it does not gate**, and the exit status says so: a Python row
# printing FAIL still leaves this recipe at 0. Criterion 4 is stated over the
# Rust worker and its **MET** is that row; giving the gate a second *gated* arm
# is a decision and needs a record, which the amendment says in as many words. A
# recipe that quietly promoted a reported number to a gate would be deciding
# that here.
#
# **Same fixture, same deletion, for `gate4`'s reason.** It writes and deletes
# `target/gate4/workers.tft` — gate 4's own file, not a copy — so "on the same
# 338 MiB file" is structural rather than promised, and running the two recipes
# in either order re-freezes instead of measuring last week's. Both arms sweep
# the *identical* query set: the stamp grid is one constant in
# `frozen_workers.rs` and is handed to the Python worker on its command line
# rather than restated there. Checked on a 3.6 MiB fixture whose history is
# shorter than the grid, where both arms report 5232 lookups of a possible 6144.
#
# **`--release`, where `just py-test`'s otherwise identical install line is not.**
# PHASE5 §9.3's memory axis fails on a debug build and this row is a Pss byte
# count; it is also the profile a `pip install` gets. `py-setup` is a dependency
# for `quickstart`'s reason — a leaner venv would be a second spelling of "the
# Python environment", and it is the interpreter `just py-test` uses.
#
# **Not in a workflow, and the reason is not fidelity.** Pss reads as honestly
# on a shared runner as on a quiet one — that is why `nightly.yml` runs
# `just gate4` — so this is wireable as it stands. What it would cost is a
# Python toolchain and a second release build on top of the one that job already
# pays for, to re-derive a number that gates nothing. The record that gives
# criterion 4 a second arm is what would make it worth a job.
#
# **`--py-worker` is passed although the binary has that exact default.** The
# default is `CARGO_MANIFEST_DIR` baked in at compile time, which is the build
# machine's checkout — right for a hand-run, wrong for a binary carried
# anywhere else — and a recipe naming the file is also how somebody reading the
# justfile learns that a second, non-cargo artifact is in the loop.
#
# ~340 MiB of disk for the fixture, and 466 MiB of summed Pss across the sixteen
# workers at the moment it is sampled. `/usr/bin/time` on the recipe reports
# ~635 MB peak RSS, which is the driver building the arena it freezes and not
# the workers at all. 4.2-4.4 s wall with the venv and both release builds warm.
gate4-python: py-setup
    cargo build --release -q --features shm -p tf_tree_bench --bin frozen_workers
    VIRTUAL_ENV=.venv .venv/bin/maturin develop --uv -q --release
    rm -f target/gate4/workers.tft
    ./target/release/frozen_workers --tft target/gate4/workers.tft --workers 1,16 \
        --python .venv/bin/python \
        --py-worker crates/tf_tree_bench/python/gate4_worker.py

# **PHASE4 §7 gate criterion 1: what the C ABI costs a caller.**
#
# This recipe exists because the gate did not have one. `examples/abi_cost.rs`
# was named in a comment and executed by nothing — no recipe, no workflow — so
# `docs/PHASE4.md` carried "1.020×, PASS" as a frozen historical reading while
# the example itself had started printing FAIL.
#
# **Two builds, and the second one is the gate.** The workspace `release`
# profile is `lto = "thin"`, which inlines `tft_plan_at` into this Rust caller —
# so the boundary the gate exists to price is *not in that binary*.
# `report.rs`'s §9.2 embedding row already says thin LTO "is exactly what erases
# the boundary"; nothing had applied it to §7. `[profile.embedder]` is
# `lto = false` and is the honest one, and `just embed-cost` builds there for the
# same reason. The `release` run is kept because the contrast between the two is
# the finding, and because deleting it would leave nobody able to check the
# claim.
#
# **Pinned**, for `cpp-bench`'s reason: an unpinned run migrates cores and swings
# by more than the gate allows.
#
# **Exit status is now the gate** — at the `embedder` profile only. It was not,
# and the recipe said "wire it in the commit that fixes the regression": this is
# that commit. What made the old criterion ungateable was its denominator, an
# inlined loop that moved 43% when an unrelated second `Tree::guard()` call site
# was added to the same file. The comparands are pinned now
# (`#[inline(never)]` + `black_box`) and the binary carries a standing control
# row that fails if the pin ever stops holding. The three rungs it gates and
# their allowances are `docs/decisions/0023` — draft, so read them as a proposal
# a human ratifies by merging it.
#
# **Why this is not in a workflow when `just gate4` now is**, since the two look
# like siblings — both are §7-style gate criteria that no job used to run. They
# differ in what they measure. `gate4` is Pss, which a shared runner reports as
# honestly as a quiet one. This is a *latency quotient* between two `taskset`-ed
# columns, and whether it stays resolvable under a hosted runner's neighbours is
# not something this repository has measured — and a gate that flaps is a gate
# people learn to pass by editing the gate. Wiring it needs that number first,
# from repeated runs on a runner, not from an argument. `0023` being draft is a
# second reason and not the main one: the thresholds it would gate against are
# still a proposal.
abi-cost:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -q -p tf_tree_c --features test-hooks --example abi_cost
    cargo build --profile embedder -q -p tf_tree_c --features test-hooks --example abi_cost
    echo "=== release profile (lto = \"thin\" — the boundary is ERASED; contrast only) ==="
    taskset -c 2 ./target/release/examples/abi_cost release
    echo
    echo "=== embedder profile (lto = false — a REAL boundary; THIS one gates) ==="
    taskset -c 2 ./target/embedder/examples/abi_cost embedder

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
    # **ASan, and `shm` is in this row on purpose.** `docs/decisions/0015`'s
    # shared arm maps a `memfd`, binds a unix socket and spawns the owner
    # thread; Miri can execute none of that, so the `--test bridge` row above
    # runs the heap path only and ASan is the sole sanitizer that ever sees the
    # shared one. It is also the checker that catches the trap the prefix rule
    # keeps setting: a relaxed `struct_size` test whose read was not narrowed
    # reads the whole current struct out of an older caller's shorter
    # allocation, and `tests/bridge.rs` allocates those prefixes tightly so the
    # overrun is real.
    RUSTFLAGS=-Zsanitizer=address cargo +nightly test -p tf_tree_c \
        --features test-hooks,bridge,shm --target x86_64-unknown-linux-gnu -Zbuild-std

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
# `cbindgen` is deliberately not a workspace dependency (MPL-2.0 against
# `deny.toml`'s allowlist), so it has to be on `$PATH` as a binary:
#
#     cargo install cbindgen --locked --version 0.29.4
#
# **It is needed for step 1's `--check`, not only for regeneration.** `--check`
# generates both headers and diffs them against the committed files, so without
# the binary this recipe exits 1 on its first line and the compile matrix below
# never runs. That is not hypothetical — CI's `c-surface` job shipped without
# installing it and could not have passed on a clean runner; the job now installs
# the version pinned above.
#
# The pin is a determinism measure, not a known incompatibility: cbindgen 0.28.0
# and 0.29.4 were both measured to reproduce the committed headers byte-for-byte.
# It is here because `--check` compares generated text to a committed file, so a
# future release that changes whitespace would fail every PR for a reason that is
# not a diff in `src/`. Bump it deliberately, with the regenerated headers in the
# same commit.
c-header-check:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo xtask headers --check
    # **`bridge` is in this build, and the smoke test is compiled with
    # `-DTFT_HAVE_BRIDGE`.** The bridge declarations are emitted inside
    # `#if defined(TFT_HAVE_BRIDGE)`, so without both halves the ten §5 entry
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
    # The same three-place search `cpp-check` does, in the same order, rather
    # than the hardcoded `/usr/include/eigen3` this line used to carry — which
    # would have failed on exactly the machine `cpp-deps` now provisions.
    eigen=""
    for d in /usr/include/eigen3 /usr/local/include/eigen3 target/thirdparty/eigen; do
        [ -d "$d" ] && eigen="-isystem $d" && break
    done
    [ -n "$eigen" ] || { echo "cpp-bench: Eigen not found; run \`just cpp-deps\`" >&2; exit 1; }
    # **Both error modes.** The gate applies to the wrapper, and
    # `-fno-exceptions` is a different wrapper: a first implementation of
    # `expected<T>` made an FFI call per success and missed the gate at 1.064x
    # while the exceptions build measured 1.002x. Measuring one mode and
    # reporting "gate 2 passes" was wrong, and this is the fix.
    for mode in "" "-fno-exceptions"; do
        g++ -O2 -std=c++17 $mode -Wall -Wextra -Werror -I crates/tf_tree_c/include \
            $eigen $sophus -o "$out/bench" \
            crates/tf_tree_c/tests/cpp/bench.cpp target/release/libtf_tree_c.a \
            -lpthread -ldl -lm
        taskset -c 2 "$out/bench"
        echo
    done

# Fetch the header-only C++ dependencies into target/thirdparty so `cpp-check`
# can exercise §4.2 and §4.3. Not vendored: they are test dependencies of one
# recipe, and putting somebody else's headers in the repo to test a stride is a
# poor trade.
#
# **Eigen is here because the first nightly run failed without it** (2026-08-17).
# `cpp-check` treats Eigen as a hard requirement — §4.2's interop cannot be
# exercised without it, so its absence fails rather than skips — and this recipe
# fetched only Sophus, so `ubuntu-latest`, which ships no Eigen, could not run
# the check at all. The fix belongs here rather than as an `apt-get` line in
# `nightly.yml`: a dependency spelled in a workflow is a dependency the recipe
# does not have, and a developer without sudo still could not run `just
# cpp-check`. With it here the workflow needs no change at all.
#
# Only fetched when the system has none. `run.sh` prefers `/usr/include/eigen3`,
# because §4.2 is about interop with the Eigen a consumer actually has; this is
# the bootstrap for a machine that has none.
cpp-deps:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p target/thirdparty
    if [ -d /usr/include/eigen3 ] || [ -d /usr/local/include/eigen3 ]; then
        echo "cpp-deps: Eigen already installed system-wide"
    elif [ -d target/thirdparty/eigen ]; then
        echo "cpp-deps: Eigen already fetched"
    else
        # Pinned to the version this repository measured against; 3.4.0 is also
        # what `libeigen3-dev` installs on the development host, so the fetched
        # and installed configurations are the same one.
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

# **Rustdoc, with warnings denied — the docs.rs shop window.**
#
# Nothing gated rustdoc until this recipe existed, and warnings accumulate
# silently because `cargo doc` exits 0 on every one of them. Measured before it
# was written: `cargo doc --no-deps --workspace` emitted **80** warnings — 44
# unresolved intra-doc links, 35 public items linking to private ones, one
# redundant explicit target. Every one of those renders on docs.rs as a dead
# link the moment a crate is published.
#
# **The configuration is docs.rs's, not `--workspace`'s, and that is the point.**
# The five publishable crates — `tf_tree`, `tf_tree_core`, `tf_tree_math`,
# `tf_tree_arena`, `tf_tree_ipc` — each set `all-features = true` and
# `rustdoc-args = ["--cfg", "docsrs"]` in `[package.metadata.docs.rs]`, so the
# build that renders publicly is the all-features one, and the same flags are
# passed here. That sentence was **false when it was first written**:
# `tf_tree_ipc` had no `[package.metadata.docs.rs]` block at all, so docs.rs
# would have rendered it at default features while this recipe checked it at
# all-features. The block was added rather than the claim weakened. The other
# four crates on line 1 (`tf_tree_c`, `tf_tree_cli`, `tf_tree_ingest`,
# `tf_tree_bridge`) are `publish = false` and are here because they are public
# API to *somebody* — a C caller, an operator, the ROS node.
#
# `--cfg docsrs` buys nothing today: no source file in the workspace reads it
# (`rg 'docsrs|doc_cfg' crates/*/src` is empty). It is passed because docs.rs
# passes it, so the day a `#[cfg_attr(docsrs, doc(cfg(...)))]` lands, this
# recipe is already checking the configuration that renders.
#
# **Line 2 is `publish = false` and needs its own feature set.** `--all-features`
# on `tf_tree_bench` enables `tf2`, whose build script needs a ROS 2 install no
# host recipe has, so the features are named instead: `shm` and `embed-probe`
# are enabled and `tf2` is not. That is not cosmetic — 9 of `tf_tree_bench`'s 13
# `required-features` targets are documentable binaries, and at default features
# rustdoc sees none of them nor `src/shm_util.rs`. Measured: a broken intra-doc
# link injected into `shm_util.rs` left `cargo doc --no-deps -p tf_tree_bench`
# exiting 0 and fails the line below. `xtask` has no features at all.
#
# `shm` makes line 2 Linux-only, exactly as `just shm-check` already is.
#
# **What this deliberately does NOT gate**, and there are two:
#
# * `tf_tree_bench`'s `tf2` feature — `src/tf2.rs`, `src/replay_tf2.rs`, the
#   `tf2_scaling` binary and the `tf2_compare` bench. That code needs the
#   container, and `just tf2-check` is where it is compiled and linted.
# * A plain default-feature `cargo doc --no-deps -p tf_tree` still reports 2
#   unresolved links — `Tree::open_frozen` and `crate::open`, both
#   `#[cfg(all(feature = "shm", target_os = "linux"))]`. They resolve in the
#   build docs.rs performs, and de-linking them would trade two working links in
#   the rendered documentation for a clean run of a command that is not the gate.
#
# **And the two excluded crates, which no `-p` here can name.** Both are in the
# root manifest's `exclude`, so `cargo doc --no-deps -p tf_tree_py` answers
# *"package ID specification `tf_tree_py` did not match any packages"* and the
# only spelling that reaches either is `--manifest-path`:
#
# * `tf_tree_py` — the crate whose documentation a PyPI user reads, and the one
#   this recipe would most like to cover. It is gated by `just py-lint`, whose
#   rustdoc line carries the argument for living there: the `--manifest-path`
#   form works fine from here, but PyO3's build script needs an interpreter and
#   `py-*` is what owns one.
# * `tf_tree_tf2_sys` — rustdoc for it runs in **no** recipe. It needs ROS 2
#   headers, so the only recipe that could carry it is `just tf2-check`'s
#   container invocation, and it is `publish = false` behind
#   `tf_tree_bench --features tf2`, so nothing it says renders on docs.rs. That
#   is a smaller hole than `tf_tree_py`'s was, and it is stated here rather than
#   left to be rediscovered.
doc:
    RUSTDOCFLAGS='-D warnings --cfg docsrs' cargo doc --no-deps --all-features \
        -p tf_tree -p tf_tree_core -p tf_tree_math -p tf_tree_arena \
        -p tf_tree_ipc -p tf_tree_c -p tf_tree_cli -p tf_tree_ingest \
        -p tf_tree_bridge
    RUSTDOCFLAGS='-D warnings' cargo doc --no-deps -p tf_tree_bench \
        --features shm,embed-probe
    RUSTDOCFLAGS='-D warnings' cargo doc --no-deps -p xtask

# **`evidence-audit` and `artifact-versions` are dependencies, not lines in the
# body, and the difference is a rule this repository already has.** Both used to
# be spelled here a second time as `./scripts/…`, beside the recipe of the same
# name — two spellings of one path, which is the thing `docs/PROJECT.md` §6 says
# not to do. It also made both recipes look orphaned to anyone auditing which
# `just --summary` entries a workflow reaches: no workflow names either one, and
# the only reason they run on every pull request is that `ci.yml`'s `lint` job
# runs this recipe. Naming them here says so in the one place that cannot drift.
#
# They stay first, and stay in this order, because they are the cheapest things
# in the file and one of them caught a real defect: PHASE4 §7 gate criterion 1
# was recorded as PASS for months while the benchmark that produces it ran in no
# recipe at all. `just` runs dependencies left to right and before the body, so
# ordering survives the move — `no-build-output`, then `py-compile`, then the
# artifact audit, then the version audit, then eight clippy passes.
#
# `no-build-output` goes first because it is both the cheapest (6 ms measured,
# 5 ms in CI) and the
# one whose failure invalidates the rest: if the tree has build output committed
# in it, what clippy thinks of the source is not the interesting news.

# fmt + eight clippy configurations, behind the three cheap audits.
lint: no-build-output no-conflict-markers py-compile evidence-audit artifact-versions
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
    # **`pure-hash`, because every pass above compiles it out.** The feature is
    # off by default and swaps `blake3`'s backend, so nothing else here builds a
    # line of it — the same shape as the five rows above, and the reason this
    # recipe has more than one pass at all.
    #
    # This is a *lint* row and not the row that proves the feature does its job:
    # what it buys is a cross-check to `*-apple-darwin` / `*-windows-msvc`, and
    # that needs the target installed. `ci.yml`'s `bindings-non-linux` is where
    # that belongs.
    cargo clippy -p tf_tree_core --features pure-hash --all-targets -- -D warnings
    cargo clippy -p tf_tree --features pure-hash --all-targets -- -D warnings
    # **`crash-points` (`docs/PHASE2.md` §11.3), for the same reason as every row
    # above it.** The feature is default-off and places named `abort()` sites in
    # the mutation protocols; the workspace pass compiles all of it out, so
    # without this row the module and its eight feature-gated tests are code no
    # gate can see — the state `tf_tree_py` was in when it shipped a
    # `transmute` that silently discarded a claim lease.
    #
    # Two passes, not one, because the feature takes `std` for itself
    # (`#[cfg(any(test, feature = "crash-points"))] extern crate std;`) and the
    # crate is `#![no_std]` unconditionally. The `--no-default-features` arm is
    # what catches a `crash.rs` edit that reaches for something only `alloc`
    # plus a default feature provides.
    cargo clippy -p tf_tree_core --features crash-points --all-targets -- -D warnings
    cargo clippy -p tf_tree_core --no-default-features --features crash-points --all-targets -- -D warnings

# **`tf_tree_py` is excluded from the workspace, so nothing else builds it.**
#
# That gap shipped a real bug: `PyPublisher` held a
# `transmute::<EdgeWriter, Publisher>` which compiled only while the two types
# happened to be the same size, and which silently discarded the claim lease and
# the fork guard. It went unnoticed across six PRs because `just test` and
# `just lint` never compiled the crate at all.
#
# **What it needs is an interpreter, and a venv is only one way to have one.**
# Until this recipe said so it skipped on every clean checkout — which is every
# CI runner and every first clone — so the gate `lint` depends on was absent in
# exactly the configuration CI runs, and `ci.yml`'s `bindings` job covered the
# hole by re-spelling the two lines below. That job now invokes this recipe.
# PyO3 needs a Python to *run*, not to link against: its build script executes
# the interpreter to read a configuration out of it, and pyo3-ffi declares the C
# API in Rust rather than including a header. Measured on a host with no
# `/usr/include/python3.12/Python.h` and no `.venv`, from an emptied
# `crates/tf_tree_py/target`: this recipe compiled every dependency and finished
# clean against `/usr/bin/python3` in 13.12 s.
#
# The venv still wins where there is one, so this recipe and `just py-lint`
# compile one PyO3 configuration into one target directory instead of thrashing
# it between two. The skip survives for the only case that genuinely cannot
# compile: no interpreter at all.
#
# **`cargo fmt` lives here because it needs neither a venv nor an interpreter,
# and because `lint`'s `cargo fmt --all -- --check` does not reach this crate**
# — `--all` is every workspace *member*, and this one is excluded. Measured: a
# mangled `fn    _fmt_probe( ) ->u32{ 1 }` appended to
# `crates/tf_tree_py/src/lib.rs` (restored byte-for-byte afterwards) left
# `cargo fmt --all -- --check` exiting 0, and failed the line below.

# fmt + clippy for `tf_tree_py`, which no workspace command compiles.
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
    # `--all` is every workspace *member*, and `tf_tree_py` is excluded — so
    # without this line `just lint` fails, through `py-compile`, on a file the
    # recipe that exists to fix formatting leaves untouched. Only fmt: the
    # clippy half needs an interpreter, and `py-compile` is where that branch
    # lives.
    cargo fmt --manifest-path crates/tf_tree_py/Cargo.toml
    cargo clippy --workspace --all-targets --fix --allow-dirty -- -D warnings

# cargo-deny: advisories, licenses, bans, sources.
audit:
    cargo deny check

# **The MSRV floor, on the host rather than only in CI.**
#
# `SUPPORT.md` calls the floor "enforced, not intended", and until this recipe
# existed the only thing enforcing it was CI's `msrv` job — which produced no run
# between 2026-07-23 and 2026-08-16. A floor whose only gate is a workflow that
# may stop running without anyone noticing is
# back to being intended, which is the exact failure that took `rust-version` from
# 1.83 to 1.85: the number looked authoritative and nothing had ever compiled
# against it.
#
# **The job no longer mirrors this recipe; it runs it.** It used to be a
# transcription of two of the three arms below, and the transcription had also
# dropped the `+$want` from the build — which, with `rust-toolchain.toml` pinning
# `channel = "stable"`, meant the step that was supposed to compile on the floor
# compiled on stable. The workflow now installs the floor's toolchain (which is
# the one thing this recipe cannot do for itself, since it refuses to fall back
# to stable) and then invokes `just msrv`.
#
# The version is read out of the manifest rather than written here, the
# `--locked` build uses the committed lockfile (a transitive crate that quietly
# needs a newer toolchain is the drift being caught, so re-resolving would hide
# it), and `--lib --bins` because the promise covers what a downstream *links*,
# not what our dev-dependencies need.
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
    # `CLAUDE.md` is on the list because it states the floor too, and because an
    # agent reads it before it reads anything else — a wrong number there is
    # acted on rather than merely believed. It was ungated until 2026-08-17,
    # found while correcting the neighbouring line, which claimed version 0.0.1
    # unpublished two versions and one publish later.
    #
    # The five publishable crates' `README.md` are on the list because each is
    # rendered as a crates.io front page and each states the floor — and none of
    # them was checked until 2026-08-22, found while closing #238 about the
    # neighbouring defect in the same five files. Same class, same files, and the
    # front page is where an adopter actually reads the number.
    #
    # **This arm tests presence, not absence.** A document that states the right
    # floor and a wrong one alongside it passes, demonstrated on a copy. That is a
    # real gap and it is not this recipe's to close by loosening the match.
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

# **The repository checked against itself: one version everywhere, and no
# document that names a recipe which does not exist.**
#
# Third of the family that starts with `just msrv` and `just evidence-audit`,
# and it exists because three independent readings of this release found the
# same defect in three different places — shipped text contradicting a document
# that same text names as authoritative. The README's status table against the
# `§0.0` tables it calls the source of truth; the CLI's `--help`, still saying
# "live external attach arrives in Phase 2" two phases after it arrived; and a
# README quickstart that said `just py-wheel` was "build + install" when the
# recipe only built, so the documented first five minutes ended in
# `ImportError`. None of the three is a property of the code, which is why no
# test caught any of them.
#
# **The version half is the gap `just msrv` leaves.** That recipe does exactly
# this for one field: it reads `rust-version` out of the manifest and fails if a
# hand-written copy — or the README's prose — disagrees. There was no equivalent
# for `version`, and **nine** files carry a hand-kept copy of it: two manifests
# outside `[workspace]` (they cannot inherit), `pyproject.toml`, three
# `CMakeLists.txt` and two `package.xml`. `crates/tf_tree_c/CMakeLists.txt`'s
# own comment said as much — "Unlike `rust-version`, nothing compares these
# copies … this is the convention, not a gate" — and this is that sentence
# stopping being true.
#
# **What it deliberately does not check is the README's status table**, which is
# where the first of the three findings was. No cheap rule separates a stale row
# from a differently-worded true one, and `docs/PHASE5.md` §10's point about the
# benchmark baseline applies here: a gate that flaps is a gate people learn to
# pass by editing the gate. So every rule in the script was measured over the
# whole corpus before it was written down and narrowed until it had no false
# positives — the recipe-reference arm resolves 242 references across 19
# documents and 49 across 3 workflows, and the single finding it produced on the
# tree it was written against was `just quickstart`, which did not exist yet.
# `docs/decisions/` is out of its scope for the same reason: a `ready` record is
# a dated artifact, and renaming a recipe must not force an edit to history.
#
# **The newest arm is a rendering check, and it is here because a document can
# be wrong in a way no reader can see.** GFM drops every cell past the header's
# column count and warns nobody: `docs/PHASE2.md` §12.2 carried two three-cell
# rows in a two-column table for five days (#208), so a benchmark's whole result
# — and a figure that had since gone stale — rendered as nothing at all on
# github.com while looking right in every editor and every diff. `docs/API.md`
# row 16 was the same defect from unescaped pipes inside `|s| ≈ 2.3`, and the
# commit that claimed to fix it did not. Escape-aware, because `\|` is
# legitimate content — a naive pipe count false-positives on §3.6's `SHRINK\|GROW`
# row — and it scans every tracked Markdown file, `docs/decisions/` included: a
# ragged row is not a rename, it is a defect the document had the day it was
# written.
#
# **It also has to not fire on a document that is right**, which is the half a
# first draft missed: three constructions hold a pipe table that GFM renders as
# something else — a setext `---` heading under a line with a pipe in it, a
# four-space-indented code block, and an HTML comment (`CHANGELOG.md` opens
# one). Each is skipped, each was verified by writing it and watching the check
# stay silent, and the indented-code rule counts four spaces past the innermost
# list item's content so that `0005`'s real table inside item 11 keeps being
# checked. A gate that blocks a correct document is a gate somebody removes.
#
# Wired into `just lint` (0.14-0.15 s with the table arm, up from 0.10 s for the
# four arms without it; no network, three runs byte-identical).

# One version across the repository, no document naming a recipe that is not
# there, and no table row GFM would silently truncate.
#
# **The table check enumerates with `git ls-files '*.md'`, so a document you have
# not staged yet is not checked at all** — a new record passes this recipe
# trivially until `git add`. CI never sees that, because it checks out a tree in
# which everything is tracked; the false green is local and it is loudest exactly
# when you are adding the document whose table you want checked. `git add -A`
# first, then run this.
artifact-versions:
    ./scripts/artifact-versions.py

# Run the benchmark suite and the go/no-go gate.
bench:
    cargo xtask bench-gate

# **What the `0036` receipt-time sampler costs a publisher.** Reports; does not
# gate — there is no pass/fail criterion in any document, only a number that has
# to stay honest.
#
# It is a separate recipe from `just bench` because the question is a *delta* and
# this host cannot produce one any other way. `bench_report`'s fitness probe
# rejects it outright (SMT on, 8 logical CPUs over 4 physical cores, no readable
# frequency governor), and two `cargo bench` runs minutes apart drift by more
# than the effect: the same unsampled push read 5.94 ns and then 4.82 ns while
# the effect under test was ~1.1 ns. A before/after across those two runs said
# +47%; the paired arms this bench runs back to back in one process said +23%,
# five times. **Run this, not a before/after, when the sampler changes.**
push-sampler-cost:
    cargo bench -p tf_tree_bench --bench push_sampler

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
# The `tf_tree_c --features bridge,shm` clippy row is **the feature set
# `ros/build.sh` builds**, and that is the whole of why it is here.
# `ros/tf_tree_ros` links a `libtf_tree_c.a` built by *this image's* rustup
# toolchain, which is installed independently of the host's and is pinned only by
# the Dockerfile's `RUST_TOOLCHAIN` argument. This is the recipe to run before
# `just ros-build`, and it is where a container-side toolchain drift in the one
# crate the ROS package links shows up.
#
# It is deliberately **not** the same command `just lint` runs, and used to be:
# `lint`'s row is `--features bridge` and `just shm-check`'s rows are the `shm`
# ones, both on the *host* toolchain — which is the one thing this recipe exists
# not to trust. A row here that named a feature set nobody builds in this image
# would leave `bridge,shm` — `ros/build.sh` step 1, `docs/decisions/0015`'s
# combination — compiled by no linting recipe on either side.
#
# **The §9.2 artifact with the tf2 columns compiled in, and its own baseline.**
#
# `just bench-check` builds without `--features tf2`, so every row needing a
# `tf2::BufferCore` is `unavailable` there for a *build* reason — correctly, and
# it says so. That leaves the project's central performance claim gated by
# nothing, which is what this pair fixes.
#
# **Two baselines, not one, and they must not be merged.** The status comparison
# is one-directional: a row `measured` in the committed baseline and not
# `measured` now is a withdrawn claim and a hard failure. A single baseline cut
# with `tf2` would therefore make `just bench-check` fail on every host without
# ROS 2 — on the difference between two recipes rather than on the code, which is
# exactly the trap `bench-check`'s own comment documents for `--embed-cost`.
# So each recipe checks the baseline cut by the matching build.
#
# `lookup_ratio_vs_tf2` is the row that resolves here and nowhere else. It is a
# `Sensitivity::Ratio` row, so the fitness probe's timing verdict does not reach
# it: the arms are interleaved within every round, which is what makes ~2.5x
# resolvable to a ~3% band on a host whose absolute latencies are unusable.
tf2-bench-report *ARGS:
    ./docker/tf2/run.sh 'cargo run --release -p tf_tree_bench --features tf2 --bin bench_report -- {{ARGS}}'

# The tf2-side regression gate. Container-only, like everything else here.
tf2-bench-check:
    ./docker/tf2/run.sh 'cargo run --release -p tf_tree_bench --features tf2 --bin bench_report -- \
        --out target/tf2-bench-report \
        --check-baseline crates/tf_tree_bench/baseline/results-tf2.json'

# Regenerate the tf2-side baseline. Same rule as `bench-baseline-update`: run it
# deliberately, and put the diff in the commit that causes it.
tf2-bench-baseline-update:
    ./docker/tf2/run.sh 'cargo run --release -p tf_tree_bench --features tf2 --bin bench_report -- \
        --out target/tf2-bench-report'
    cp target/tf2-bench-report/results.json crates/tf_tree_bench/baseline/results-tf2.json

# **Which consumer build does the gated ratio speak for? Both, measured.**
#
# `tf2-bench-check` above builds with `cargo run --release`, so its
# `lookup_ratio_vs_tf2` row is taken under *this workspace's*
# `[profile.release]` — `lto = "thin"`, which inlines `Plan::at` across the
# `tf_tree` crate boundary into the harness. A consumer does not get that build:
# cargo applies the **top-level** package's profile to the whole dependency
# graph, and cargo's own release defaults set no LTO. `[profile.embedder]` is
# those defaults written out field by field.
#
# So this runs the same paired harness twice, once per profile, and prints both.
# **Read the tf2 column, not just the quotient**: that arm goes through
# `tf_tree_tf2_sys`' C++ shim, which no Rust LTO setting can inline into, so it
# should barely move between the two builds. If it does move, the two runs are
# not comparable and the quotient of quotients means nothing.
#
# Pinned to one core, for `cpp-bench`'s reason: an unpinned run migrates and
# swings the absolute columns, which are the thing being compared across runs
# here (the within-run quotient survives migration; a cross-run column does not).
#
# Not gated and deliberately not wired into `bench_report`: `bench_report`'s
# baseline is per-profile by construction (`runstore::BUILD_CRITICAL_FACTS`
# refuses to compare across `build_profile`), so a second profile is a second
# baseline, and nothing yet says which one the project claims.
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
tf2-check:
    ./docker/tf2/run.sh 'set -euo pipefail; \
        cargo fmt --manifest-path crates/tf_tree_tf2_sys/Cargo.toml -- --check; \
        cargo clippy --manifest-path crates/tf_tree_tf2_sys/Cargo.toml --all-targets -- -D warnings; \
        cargo nextest run --manifest-path crates/tf_tree_tf2_sys/Cargo.toml --release; \
        cargo clippy -p tf_tree_bench --features tf2 --all-targets -- -D warnings; \
        cargo nextest run -p tf_tree_bench --features tf2 --release --lib --no-tests=pass; \
        cargo clippy -p tf_tree_c --features bridge,shm --all-targets -- -D warnings'

# **The ROS 2 ingest bridge (`docs/PHASE4.md` §5), built in the container.**
#
# `ros/tf_tree_ros` is an `ament_cmake` package, not a cargo crate: it needs
# `rclcpp`, which exists only in `docker/tf2`. It therefore inherits
# `tf_tree_tf2_sys`' problem — no `cargo fmt`, no `clippy`, no `nextest` — and
# these two recipes are the whole of its gate. Run them after touching anything
# under `ros/`.
#
# `ros/build.sh` says what the three steps are and which of them is easy to get
# wrong; the short version is that the staticlib must carry
# `--features bridge,shm` — `bridge` for the nine §5 entry points, `shm` for
# `docs/decisions/0015`'s `tft_tree_open`, and the script checks one symbol per
# feature because they fail in different places — and that colcon must be told
# every output directory or it litters the repo root.
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
# **Four arms since `docs/decisions/0015`**, where there were three: the fourth is
# one bridge process publishing a shared arena plus N processes attached to it
# read-only, which is §9.1's actual sentence and the arm this project's central
# claim is about. It used to be unconstructible — the bridge built a heap arena —
# and the report printed that gap above its own table on every run instead. What
# replaced the disclosure is an accounting rule: the bridge process reports
# `consumers 0`, so its CPU and PSS land in the arm it serves rather than beside
# it. `crates/tf_tree_bench/tests/dds_report_aggregate.rs` is what pins both.
#
# Env: WORKLOAD, CONSUMERS, SECONDS_MEASURED, WARMUP, HZ, BRIDGE_LINGER,
# TF_TREE_NAME.
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
    # **`owner_migration`'s unit tests and lint, which run in no other recipe.**
    # The binary is `required-features = ["shm"]`, so `just lint`'s workspace
    # pass compiles it out entirely; without these two lines the gate that states
    # §12.3 4b would be linted by nothing and its non-vacuity test
    # (`gate_arithmetic_is_not_vacuous`) executed nowhere. The measurement itself
    # is `just owner-migration` — minutes long, and scheduler-sensitive, so it
    # does not belong in a per-branch gate.
    cargo clippy -p tf_tree_bench --features shm --bin owner_migration --all-targets -- -D warnings
    cargo nextest run -p tf_tree_bench --features shm --bin owner_migration

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
    # **The line above is the packager's shape; this one is the shape two of
    # this crate's targets are actually *run* in.** `just shm-check`'s
    # `--test frozen` line and `just shm-rendezvous` both pass
    # `shm,unstable`, and the tests those features buy —
    # `freezing_carries_the_counter_regions` and
    # `the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live`, plus the
    # helper's `join-rw-report` mode — are `#[cfg(feature = "unstable")]`, so
    # the pass above compiles them **out**. `just lint`'s workspace clippy
    # cannot reach them either: both targets carry `required-features =
    # ["shm"]`, so `--workspace` skips them whole. Without this line that code
    # is executed by a recipe and linted by nothing, which is the same hole
    # every other feature-named pass in this file exists to close.
    cargo clippy -p tf_tree --features shm,unstable --all-targets -- -D warnings
    # **And this one is the shape `just shm-rendezvous` runs**, which is not the
    # line above: `test-hooks` is what compiles `CLAIM_WINDOW_HOOK`'s call site,
    # and the pass above leaves it out. Adding the `unstable` pass without this
    # one would have left the *executed* configuration linted by nothing, which
    # is the hole being closed rather than a second copy of it. It found a real
    # defect on arrival — a `clippy::ok_expect` in `the_acquire_window_backs_out`
    # that had never been compiled under `-D warnings` by any recipe.
    cargo clippy -p tf_tree --features shm,test-hooks,unstable --all-targets -- -D warnings
    cargo clippy -p tf_tree_ipc --all-targets -- -D warnings
    cargo clippy -p tf_tree_bench --features shm --all-targets -- -D warnings
    cargo clippy -p tf_tree_cli --features shm --all-targets -- -D warnings
    # **`docs/decisions/0015`: the bridge fills a shared arena, and until this
    # line nothing in the repository built `bridge,shm` together.** They are
    # independent cargo features, so `tft_bridge_options::arena_name` had a
    # `tf_tree::Open` behind it in a configuration no recipe compiled — and
    # `tests/bridge_shared.rs`, which is the whole point of the record (a second
    # attach reading what the bridge wrote), is `#![cfg]`-ed out of every other
    # one. Not `just test-rust`: `shm` is Linux-only and that recipe runs on the
    # aarch64 matrix too. Not a new recipe either — a third feature-combination
    # recipe is a third thing to forget, and this one already carries
    # `-p tf_tree_cli --features shm --test attach` for exactly the same reason.
    #
    # The plain `--features bridge` clippy line in `just lint` stays: since the
    # record, `bridge`-without-`shm` is a shipped configuration with its own
    # refusal arm, so both halves need compiling.
    cargo clippy -p tf_tree_c --features bridge,shm --all-targets -- -D warnings
    cargo nextest run -p tf_tree_c --features bridge,shm
    cargo build --features shm -p tf_tree_bench --bin shm_child
    cargo build --features shm -p tf_tree_bench --bin fork_child
    cargo nextest run -p tf_tree_bench --features shm --test multiprocess
    # **`src/backing.rs`'s unit tests, which run in no other recipe.** The module
    # is `#[cfg]`-ed on `shm` — it compares a heap arena against `build_shared`,
    # so without the feature there is no second backing — and `just test`'s
    # `--workspace` builds default features, so its tests would otherwise be
    # compiled by the clippy line above and executed nowhere. They are the guards
    # that stop `just abi-split` reading a point estimate off a band that
    # contains the null, which is the failure mode the module exists to avoid.
    # `--lib` and not the whole package: the integration targets are named
    # individually, on purpose.
    cargo nextest run -p tf_tree_bench --features shm --lib
    # `abi-probe` = `bridge` + `tf_tree_c/test-hooks`, the only configuration in
    # which `abi_attached` compiles. Without this line the binary that measures
    # the C ABI boundary is linted by nothing — the same hole `just lint`'s
    # feature-named clippy passes exist to close.
    cargo clippy -p tf_tree_bench --features abi-probe --all-targets -- -D warnings
    # Fork poisoning (`docs/decisions/0005` step 9). Separate from
    # `shm-rendezvous` because it needs no second executable and no scratch
    # rendezvous beyond its own: the second process is a `fork` of the first.
    cargo nextest run -p tf_tree_bench --features shm --test fork
    # **`docs/decisions/0015`'s *Invariants to maintain*: the same `fork()`, one
    # layer up.** The line above runs `tests/fork.rs` with `bridge` **off**,
    # which is a real configuration and stays covered — but it compiles
    # `fork_child`'s fourth mode and this crate's whole `tf_tree_c` edge out, so
    # under it the C ABI is forked by nothing. `bridge` implies `shm` in
    # `crates/tf_tree_bench/Cargo.toml`, so `--features bridge` alone would do;
    # both are named because every other line in this recipe names `shm` and one
    # that did not would read as an oversight.
    #
    # The clippy line is not optional garnish: `just lint`'s `--workspace
    # --all-targets` pass builds default features, so without it the fourth
    # mode's `unsafe` blocks and its C interop are linted in **no** recipe at
    # all — that one is coverage.
    #
    # The `cargo build` line adds **no coverage** and is not pretending to: the
    # `nextest` line below must build the binary anyway, to set
    # `CARGO_BIN_EXE_fork_child`. It is here for the message — a build failure
    # naming the binary beats one surfacing as a missing environment variable
    # three processes later — and it mirrors its `--features shm` sibling four
    # lines up for the same reason.
    cargo clippy -p tf_tree_bench --features shm,bridge --all-targets -- -D warnings
    cargo build --features shm,bridge -p tf_tree_bench --bin fork_child
    cargo nextest run -p tf_tree_bench --features shm,bridge --test fork
    # §7.1 page population. `nextest` runs each test in its own process, which
    # this needs: the measurements are RSS and minor-fault deltas, and threads
    # sharing a process would read each other's.
    cargo nextest run -p tf_tree_bench --features shm --test population
    # **`tf_tree_cli`'s unit tests under `shm`, which ran in no recipe.** The
    # lines below name integration targets one by one, and `just test`'s
    # `--workspace` builds default features — so every `#[cfg(feature = "shm")]`
    # unit test in `src/` was compiled by the clippy line above and executed
    # nowhere. `lib.rs`'s `recorded_given` is one: it is the `/proc`
    # classification `TFT014`'s fork arm rests on, and its arms are only
    # assertable by passing the two host facts in, which is a unit test's job
    # (`docs/decisions/0028` plan step 6). `--lib` and not the whole package:
    # the integration targets are named individually, on purpose.
    cargo nextest run -p tf_tree_cli --features shm --lib
    # The CLI against a live arena, and `participants` against no arena at all.
    # This is the milestone's acceptance test: the shipped binary, through clap,
    # joining somebody else's tree.
    cargo nextest run -p tf_tree_cli --features shm --test attach
    # §7's `--web` view under the same feature. `--workspace` runs `tests/web.rs`
    # without `shm`, and that is a different binary: `cmd_top_web` calls the
    # `merge` closure that only exists under `shm`, so the build an operator
    # actually attaches with was compiled by clippy here and executed nowhere.
    cargo nextest run -p tf_tree_cli --features shm --test web
    # **`doctor --from-file` (`docs/PHASE5.md` §6), which needs the frozen
    # backend and therefore `--features shm`.** The `--from-bag` half of the
    # same feature runs under `just test` — ingest needs no features — so
    # without this line only one of `doctor`'s two recording sources would be
    # gated, and the one left out is the one that carries the *skip* proving
    # `TFT018`/`TFT019` do not pass vacuously on a `.tft`.
    cargo nextest run -p tf_tree_cli --features shm --test doctor_frozen
    # **`doctor`'s resolved runtime directory (`docs/PHASE2.md` §15).** The
    # directory is where the *rendezvous* looks, so without `shm` there is none
    # and the field is correctly absent — which makes the whole file `#[cfg]`-ed
    # out of `just test` and gated only here.
    cargo nextest run -p tf_tree_cli --features shm --test doctor_runtime_dir
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
    # **`unstable` on the line below buys exactly one test, and without it that
    # test runs nowhere.** `freezing_carries_the_counter_regions` (§2's
    # counter-region carry-over) is `#[cfg(feature = "unstable")]` — all three of
    # its arena reads go through `Tree::arena_view`, so there is no stable-tier
    # spelling of it. Every *other* `unstable`-gated test in this crate is reached
    # by `cargo nextest run --workspace`, where the resolver unifies the feature
    # in from `tf_tree_cli`/`tf_tree_c`/`tf_tree_bench`/`tf_tree_py`; this target
    # carries `required-features = ["shm"]`, so `--workspace` skips it whole and
    # this line is the only one that can run it. Until 0.0.1 the facade's
    # self-dev-dependency turned the feature on here for free; deleting it (see
    # `crates/tf_tree/Cargo.toml`) took the test out of every recipe at once and
    # `--features shm` alone would leave it there. Measured:
    # `cargo nextest list -p tf_tree --features shm --test frozen` lists 8 tests,
    # `--features shm,unstable` lists 9.
    #
    # The clippy line at the top of this recipe stays `--features shm` alone on
    # purpose: that is what compiles this crate's test targets with `unstable`
    # *off*, the shape a packager building the tarball gets.
    cargo nextest run -p tf_tree --features shm,unstable --test frozen
    # **`docs/PHASE2.md` §11.3's crash matrix — four rows now, not one.**
    # `takeover.after_ownership_lock_before_bind` (the row §3.5 owes),
    # `topo.holding_lock`, `open.after_ownership_lock_before_bind` and
    # `open.after_create_before_bind`. Three features and none is optional: `shm`
    # for the rendezvous, `unstable` because `rendezvous_child`'s
    # `join-rw-report` arm needs `Tree::arena_view`, and `crash-points` because
    # without it the site is compiled out and the armed child never dies — which
    # would leave `a_killed_heir_leaves_the_role_for_the_next_survivor` waiting
    # on a `wait()` that never returns rather than failing. Every other recipe
    # compiles at least one of the three out, so this line is the only one that
    # runs it, which is the same argument the `--test frozen` line above makes
    # about `unstable`.
    cargo nextest run -p tf_tree --features shm,unstable,crash-points --test rendezvous
    cargo clippy -p tf_tree --features shm,unstable,crash-points --all-targets -- -D warnings
    # **`docs/decisions/0017` steps 2 and 3 — and this line is the rule three
    # paragraphs above being obeyed rather than restated.** Half of
    # `tests/owned_writer.rs` is `#[cfg(all(feature = "shm", target_os =
    # "linux"))]`, because a claim *lease* is an OFD byte in the rendezvous lock
    # file and a heap tree has no lock file at all. Those two tests are the ones
    # that reproduce the shipped `tf_tree_py` defect — a leaked lease, so the
    # edge is permanently unclaimable and invisible to every reaper — and
    # `--workspace` compiles them out. The other half runs under `just test`.
    cargo nextest run -p tf_tree --features shm --test owned_writer
    # **The facade's own unit tests, under `shm`, which ran in no recipe.** The
    # two lines above name integration *targets*; `--lib` was missing, so a
    # `#[cfg(feature = "shm")]` unit test inside `crates/tf_tree/src` was
    # compiled by this recipe's clippy line and executed by nothing — the same
    # hole the `-p tf_tree_bench --features shm --lib` line above closes for the
    # bench crate, in the crate where it is easiest to hit. It went in with
    # `cache::tests::two_handles_on_one_shared_arena_share_their_plans`, which
    # is the only place the #196 fix's cross-handle half is checked: that two
    # `Tree`s onto one segment share an arena id, and therefore share plans
    # rather than each recompiling.
    cargo nextest run -p tf_tree --features shm --lib
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

# **§11.4's "a random crash point armed in 10% of children" — `docs/PHASE2.md`
# §11.3 and §11.4 meeting for the first time.**
#
# `shm-torture` above kills children with `SIGKILL`, which lands wherever the
# scheduler puts it: a real fault, and a much shallower set of mid-protocol
# states than §11.3's named ones. This arms a random site from
# `tf_tree_core::crash::SITES` and `tf_tree::CRASH_SITES` in about a tenth of
# children, so a run kills processes *at named instructions* and then checks the
# same invariants.
#
# **The build needs the feature and the flag refuses without it**, because the
# children are this same executable: a site compiled out here is compiled out in
# every child, and the flag would arm nothing while looking like it had.
#
# **Read the `§11.3:` line, not the exit status.** It reports children armed and
# children that aborted at a site, and those differ — an armed child the driver's
# `SIGKILL` reached first never got there. `armed N, aborted 0` is a run that
# exercised nothing, which is why both numbers are printed. Measured on this
# host at 40s/10 children/2 Hz: 11 armed, 4 aborted, at four distinct sites,
# 0 violations.
#
# Longer and gentler than the plain soak on purpose: a high kill rate wins the
# race against the site more often than not.
shm-torture-crash-points *ARGS="--duration 5m --children 10 --kill-hz 2":
    cargo build --release --features shm,crash-points -p tf_tree_bench --bin shm_torture
    ./target/release/shm_torture --crash-points {{ARGS}}

# **The torture harness's own gate**, seconds rather than minutes, so it belongs
# on a branch rather than in a nightly.
#
# It runs the binary twice and asserts the two runs disagree: once with a child
# publishing a deliberately corrupt transform (which some *other* process must
# catch) and once clean (which must pass having validated thousands of reads).
# Without it, a harness that quietly stopped reading would print the same
# "0 violations" forever — which is exactly what the first revision of this
# harness did, and how the writer pacing in `work` came to exist.
# **`docs/PHASE2.md` §12.2's two ownership-migration rows, and §12.3 gate 4b** —
# the normative criterion that had no artifact until 2026-08-29. §3.5's migration
# shipped on 2026-08-28 with correctness tests; nothing measured its *latency*,
# and `docs/benchmarks/EVIDENCE.md` had no row for 4b, so "lookup p99.9 during a
# migration within 5% of steady state, and zero failed lookups" could not be
# stated from anything.
#
# Five processes, five migrations: an owner that only serves (so killing it does
# not stop the data stream), a never-killed writer, an heir running §3.5's
# caller-driven trigger, and read-only readers that make no control-plane call at
# all. Exits non-zero on FAIL, and **separately** on INVALID — a run whose writer
# the host starved cannot state the gate, and saying so beats blaming the arena.
#
# Latency here is partly a measurement of the scheduler: run it on an idle
# machine. `--repeat` buys tail samples, which is the figure that needs them.
#
# `*ARGS` like `shm-torture-crash-points`: `--repeat` is the knob the tail figure
# needs, and a recipe that names it in its own comment must be able to pass it.
# `${CARGO_TARGET_DIR:-target}` because a set `CARGO_TARGET_DIR` sends
# `cargo build` elsewhere and a hard-coded `./target/release/` would then exec a
# stale binary or none — the trap `bench-check` and `c-header-check` already
# carry.
owner-migration *ARGS:
    cargo build --release -q --features shm -p tf_tree_bench --bin owner_migration
    "${CARGO_TARGET_DIR:-target}/release/owner_migration" {{ARGS}}

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
    # **`test-hooks` buys six tests here**, and both of the seams it carries
    # exist because the state under test cannot be stood in from outside.
    # `CLAIM_WINDOW_HOOK` is one injection point inside `Tree::claim`, between
    # the arena CAS and the lease `SETLK`: the window is a single syscall wide,
    # so `the_acquire_window_backs_out` cannot place a reaper inside it by
    # racing. The hook is inert when unset, so the other tests run as they
    # always did. `reclamation_verdict_for_test` reaches `docs/decisions/0028`
    # plan step 2's predicate, which is private and whose two production
    # callers — the owner's slot assigner (that record's plan step 3) and
    # `Tree::reap_participants` (step 5) — both *act* on a verdict without ever
    # reporting one: the assigner stops at the first grantable slot, and the
    # sweep reports a slot count and so cannot separate `Live` from `Unknown`.
    # Its five tests turn on facts only the kernel produces — a `SIGSTOP`ped
    # process keeps its lock byte, a
    # `SIGKILL`ed one loses it, a read-only joiner holds one and writes no arena
    # record — so they cannot be unit tests inside the crate. That seam also
    # reports how many times the predicate asked the kernel, which is the only
    # part of its read *order* a multiprocess test can observe.
    #
    # **`unstable` buys eleven tests here, the same trade `just shm-check`'s
    # `--test frozen` line makes.** The enumeration below names the eight that
    # existed when it was written; the other three —
    # `a_byteless_creators_record_reads_dead_and_is_reaped_while_it_publishes`,
    # `a_byteless_publisher_is_evicted_from_the_edge_it_is_publishing_to` and
    # `a_leased_publisher_keeps_its_edge_against_a_sweeper` — landed since, and
    # are named rather than absorbed because a `comm` over two `nextest list`
    # runs is how they were found and the next reader deserves the same list.
    #
    # Two of the eight drive the helper's `join-rw-report` mode, which reads a
    # participant record's raw `state` word through
    # `Tree::arena_view` — the only route to it (`docs/API.md` §2.6).
    # `the_hangup_frees_a_joiners_slot_and_leaves_the_owners_live` needs it
    # because a slot that stays `LIVE` after its process dies cannot be told
    # from one that was released by `Tree::participant_alive` alone, since that
    # predicate folds `state == LIVE` into its own answer;
    # `defect_201_a_forced_creators_record_reads_dead_while_it_is_publishing`
    # needs it because the *creator* cannot ask about itself — the probe's own
    # slot is short-circuited to alive — so the observer has to be a second
    # process reporting on somebody else's record. The third,
    # `defect_201_release_ownership_strands_a_live_non_owner_on_byte_0`, has
    # carried the gate since #221 added it, and its body names no `unstable` API
    # — it reads the arena through `Tree::participant_slot` and `tf_tree_ipc`'s
    # lock-file accessors — so that one is a gate to re-examine rather than a
    # trade. Three more are `0028`'s plan steps 3 and 4, and all three need
    # the feature to *stage* their fixture rather than to name an API:
    # `the_assigner_reclaims_a_stale_record_no_hangup_will_ever_clear`,
    # `the_assigner_collects_a_record_left_reserved_by_a_killed_registrant` and
    # `the_hangup_collects_a_record_left_reserved_by_a_killed_registrant` write
    # participant records through `Tree::arena_view`, the only route to the
    # table, because the states they stage — a record that is not `FREE` with a
    # free lock byte in a slot this owner never granted, and a `RESERVED` record
    # left by a registrant killed inside `fill_slot`'s publication window, which
    # `0028` open question 4 measured at ~12 ns — have no producer reachable
    # from public API here. The last two are plan step 5's:
    # `a_survivor_reaps_the_killed_owners_slot_which_no_hangup_can` and
    # `a_read_only_tree_reaps_no_participant_records` call public API and need
    # the feature only to *observe* — the raw `state` word is what separates "the
    # sweep collected the slot" from "the sweep left the wedge in place", and
    # `Tree::participant_alive` reads `false` for both. Without the feature all
    # eleven and the helper mode are `#[cfg]`-ed out and the recipe runs eleven
    # tests fewer, silently — eight enumerated above plus the three named at the
    # top of this comment, which is the arithmetic that made both numbers here
    # wrong until they were re-measured.
    #
    # Measured on this branch, `cargo nextest list -p tf_tree --test rendezvous`
    # per feature set: `shm` 18, `shm,unstable` 29, `shm,test-hooks` 24,
    # `shm,test-hooks,unstable` **35** — the line below. (Earlier revisions of
    # this comment said 16/18, then 16/19/22/25, then 16/21/22/27 on `0028` plan
    # step 5's branch and 17/23/23/29 on steps 3 and 4's; each was written
    # before the next tests landed, which is why the numbers here are
    # re-measured rather than added together.)
    #
    # **Two of the four were already stale before this branch touched them**,
    # and that is worth one sentence because it is the failure mode the
    # re-measuring exists to catch: at 09efc9b the same command reports
    # 17/28/23/34, not 17/25/23/31, so the `unstable` columns had drifted by
    # three while the other two were exact. This branch adds one ungated test —
    # `a_stopped_and_continued_owner_still_serves_the_rendezvous`, the `EINTR`
    # regression — which is +1 in every column and not what moved them.
    cargo nextest run -p tf_tree --features shm,test-hooks,unstable --test rendezvous

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

# **The depth-3 ratio with no Rust binding on either arm.**
#
# `just tf2-bench-check`'s `lookup_ratio_vs_tf2` row puts tf2 behind
# `tf_tree_tf2_sys`, which charges it the residual FFI boundary and therefore
# flatters `tf_tree`. This runs the other direction: tf2 native C++, `tf_tree`
# through its C ABI as a shared library, both interleaved in one process so the
# quotient still resolves on a host that cannot time either arm absolutely.
#
# Three processes, because `tft_tree_open` attaches and cannot create (D18):
# `native_arena` serves the fixture over the rendezvous and dumps the identical
# `.tfstream` the C++ side feeds to tf2. The script wires them together.
#
# **Neither this nor the Rust row is "the" answer — they bracket it.**
# `docs/benchmarks/tf2.md` states the bracket, the unpaired point estimate, and
# what is still owed to close it. Not gated: it is not wired into
# `bench_report`, so no baseline carries it yet.
# **The memory comparison with no binding on either side.**
#
# Every other memory row in this repository puts tf2 behind the Rust binding
# `tf_tree_tf2_sys`, so the process being weighed carries a Rust runtime, a Rust
# allocator and the shim on top of tf2. The one exception, `just dds-bench`, is
# dominated by rclcpp nodes at ~14 MiB each rather than by either engine. And
# `docker/tf2/native_ratio.cpp` — built precisely to remove cross-language bias
# from the *timing* comparison — measures no memory at all.
#
# This runs two processes: a C++ program linking only `libtf2`, and `footprint`'s
# unchanged `mem-tf_tree` mode. Two instruments each — `mallinfo2` (which
# compares the engines on identical terms, since C++ `operator new` bottoms out
# in `malloc`) and Pss (which is what an operator sees in `top`, and which
# `mallinfo2` cannot see).
#
# **Needs no idle machine.** Neither instrument is a clock.
#
# It prints what tf_tree costs as measured *and* what it would cost right-sized,
# because those differ by 1.5x and the gap is declared capacity nobody published
# into. It refuses to print a quotient if the two arms stored different sample
# counts.
tf2-native-footprint:
    ./docker/tf2/run.sh 'bash docker/tf2/native_footprint.sh'

tf2-native-ratio *ARGS:
    ./docker/tf2/run.sh 'bash docker/tf2/native_ratio.sh {{ARGS}}'

# **Splitting `tf2-native-ratio`'s +52% into the two things it changed at once.**
#
# The C++ arm above measures 306.7 ns where native Rust measures 201.5 ns, and it
# moved two variables together: the call crosses a shared-library boundary the
# linker cannot see across, *and* the arena is a `MAP_SHARED` memfd rather than a
# private heap allocation. `docs/benchmarks/tf2.md` called separating them owed.
#
# This is the middle arm — same native Rust API, same off-grid §11.1 sweep, on
# the same `memfd` backing the C++ side reads — so what it reports is the mapping
# alone and the residue is the boundary. Paired and interleaved for `ratio.rs`'s
# reason, with load genuinely common-mode here: both arms are the same engine on
# the same read path, so unlike the tf2 quotient there is no lock on one side
# only.
#
# **No container**, unlike everything else in this section: there is no tf2 in
# it. Runs on the host, needs only `shm`.
#
# Not gated, and the reason is in the output rather than hidden: the backing half
# is measured, the boundary half is a subtraction against a figure from another
# run, so the tool prints which row is which.
abi-split:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release -q --features shm -p tf_tree_bench --bin arena_backing --bin native_arena
    # The cross-process rung needs an arena somebody else is serving, and
    # `native_arena` is the owner that serves one. Short runtime dir by
    # necessity: the attach socket path must fit `sun_path`'s 108 bytes.
    rt=$(mktemp -d /tmp/tft-abi-split.XXXXXX); trap 'rm -rf "$rt"' EXIT
    export TF_TREE_RUNTIME_DIR="$rt" TF_TREE_NAME=abi_split
    coproc OWNER { ./target/release/native_arena --name abi_split --stream "$rt/fx.tfstream"; }
    read -r -u "${OWNER[0]}" line || { echo "the arena owner exited before it was ready" >&2; exit 1; }
    case "$line" in ready\ *) : ;; *) echo "unexpected owner greeting: $line" >&2; exit 1 ;; esac
    status=0
    ./target/release/arena_backing --attach abi_split || status=$?
    # Guarded, and `|| true` on every step: bash unsets the fd array when a
    # coproc has already exited, so a bare close fails with "ambiguous redirect"
    # and — under `set -e` — takes the script down before `exit "$status"` runs.
    if [ -n "${OWNER[1]:-}" ]; then exec {OWNER[1]}>&- || true; fi
    wait "${OWNER_PID:-}" 2>/dev/null || true
    exit "$status"

# ---------------------------------------------------------------------------
# Python bindings (docs/PHASE3.md). `tf_tree_py` is excluded from the workspace
# because it links libpython, so none of this is reachable from `just test`.
#
# Interpreters come from uv rather than the host, so the floors in PHASE3 §10.1
# are what actually gets used. 3.14 is the GIL build; 3.14t is free-threaded,
# and §7.3 requires the suite to pass on both.
# ---------------------------------------------------------------------------

# **A clean clone to a working Python REPL in one command — and it fails loudly
# if the README's own snippet stops printing what the README says it prints.**
#
# `README.md`'s "First five minutes" tells a reader to run this and then
# `.venv/bin/python`. The last step below executes *that* snippet: it is read
# out of `README.md` rather than copied into a script, and its output is
# compared against the `# ->` marker in the snippet itself. So there is no
# second copy of the quickstart to drift (`docs/PROJECT.md` §6) and no expected
# value written down anywhere but the README. The failure it replaces is the
# one that shipped — the README said `just py-wheel` installed the extension,
# `py-wheel` only builds it, and nothing anywhere ran the five minutes.
#
# **It depends on `py-setup` rather than carrying a lighter path of its own**,
# and the temptation was real: `py-setup` installs *two* interpreters, and a
# reader who has not yet decided they care pays for the free-threaded one up
# front. What settles it is what happens next. A leaner venv would be a second
# spelling of "the Python environment" (§6 again), and everything the reader
# reaches for after the REPL — `just py-test`, `just py-lint`,
# `just py-test-freethreaded` — assumes `py-setup`'s. A quickstart whose reward
# for succeeding is that the next recipe fails is not a quickstart.
#
# The install line is deliberately the same one `py-test` runs, so a broken
# install is never something only newcomers see.

# Clean clone -> a Python REPL with the extension installed, verified end to end.
quickstart: py-setup
    VIRTUAL_ENV=.venv .venv/bin/maturin develop --uv -q
    .venv/bin/python scripts/quickstart_smoke.py
    @echo ""
    @echo "==> next: .venv/bin/python   (the extension is installed in that interpreter)"

# Create both venvs and install the toolchain.
#
# **`--allow-existing`, because this recipe is a dependency and `just` re-runs a
# dependency per invocation.** `quickstart` depends on `py-setup`, so a caller
# that runs `just py-setup` and then `just quickstart` runs it twice — and
# `uv venv` on an existing directory is a hard error ("A virtual environment
# already exists at: .venv"), not a no-op. That is how it presented: ci.yml's
# `python` job does exactly that sequence and died on the second one. `--clear`
# would also work and is wrong, because it deletes and rebuilds both venvs on
# every call for no gain; `--allow-existing` reuses the directory and the
# `uv pip install` lines below still bring the toolchain up to date.
py-setup:
    uv python install 3.14 3.14t
    uv venv --python 3.14 --allow-existing .venv
    VIRTUAL_ENV=.venv uv pip install -q maturin numpy pytest ruff pyright
    uv venv --python 3.14t --allow-existing .venv-t
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

# **The fmt and clippy halves are `py-compile`, depended on rather than
# repeated.** Both lines were spelled here as well, byte for byte, and one
# recipe restating another is the same defect as a workflow restating one
# (`docs/PROJECT.md` §6). The interpreter does not change by depending on it:
# `py-compile` prefers `.venv`, and this recipe cannot run without one anyway —
# ruff and pyright below are installed there.

# fmt + lint for both languages of the binding, plus the Rust half's rustdoc.
py-lint: py-compile
    # **Rustdoc for the one crate no other recipe compiles the documentation of.**
    # `just doc` names its packages with `-p`, and this crate is in the root
    # manifest's `exclude`, so from the root `cargo doc --no-deps -p tf_tree_py`
    # answers *"package ID specification `tf_tree_py` did not match any
    # packages"*. Until this line, fmt, clippy, ruff, ruff format and pyright
    # were the whole of the crate's gate and none of them is rustdoc — for the
    # crate whose rendered documentation is the one a PyPI user reads. The 0.0.1
    # release prep found **three** broken intra-doc links here, by running this
    # command by hand.
    #
    # **The reason it is here and not in `just doc` is the interpreter, not
    # reachability** — the "an excluded crate is structurally out of `just doc`'s
    # reach" argument is too strong, and was tested rather than inherited: the
    # `--manifest-path` form below runs perfectly well from the workspace root.
    # What it needs is a Python, because PyO3's build script runs one. Measured:
    # with `PYO3_PYTHON` pointed at a venv that does not exist it exits 101
    # (*"failed to run the Python interpreter at ..."*), and with the variable
    # unset it quietly *succeeds* against the host's `python3` — 3.12 here, not
    # the venv's 3.14 — rebuilding pyo3/pyo3-ffi/numpy for that configuration in
    # 2.6 s, and 2.7 s again to come back. So a copy in `just doc` would either
    # fail on any machine without a venv, or document a different interpreter
    # than the one the bindings are tested on, and would thrash one target
    # directory between two PyO3 configurations whenever the two recipes
    # alternate. CI already splits the same way: the `docs` job that runs
    # `just doc` provisions no interpreter, while the `python` job runs
    # `just py-setup` immediately before this recipe.
    #
    # Verified to be a real gate, by breaking it: a `[NoSuchItem]` intra-doc link
    # added to the top of `crates/tf_tree_py/src/lib.rs` (and restored
    # byte-for-byte afterwards) gives *"error: unresolved link to `NoSuchItem`"*
    # and *"error: could not document `tf_tree_py`"*, exit 101. Warm it costs
    # 0.09 s, which is why it is a line in this recipe and not a recipe of its own.
    PYO3_PYTHON=$PWD/.venv/bin/python RUSTDOCFLAGS="-D warnings" cargo doc \
        --manifest-path crates/tf_tree_py/Cargo.toml --no-deps
    # **`scripts/` is in the list because it was in no list.** Three files —
    # `artifact-versions.py` among them, which is `just lint`'s own gate — were
    # linted by nothing at all: not ruff, not pyright, not a test. Adding them
    # cost one `E501` and two reformattings, both pre-existing. Not under
    # `pyright` as well: that runs `--strict` over the *package*, and
    # `bag_to_tfstream.py` imports `rosbag2_py`, `rclpy` and `tf2_msgs` at the
    # top — a wall of missing-import errors for a ROS 2 environment the venv is
    # not and does not become.
    .venv/bin/ruff check python tests/python crates/tf_tree_bench/python scripts
    .venv/bin/ruff format --check python tests/python crates/tf_tree_bench/python scripts
    # `--strict` over the package and its stubs (PHASE3 §9). Not over
    # tests/: numpy's own stubs are partially typed, so strict there reports
    # ~120 errors that are numpy's and not ours, and a gate nobody can keep
    # green is a gate nobody runs.
    .venv/bin/pyright python

# **Builds a wheel. Does not install one — `just quickstart` is what installs.**
#
# That sentence is here because its absence shipped a bug: `README.md`
# documented this recipe as "maturin build + install into .venv", so the
# quickstart it was part of ended in `ImportError`. The README now points at
# `just quickstart`, and this comment is the other half of making that stay
# true.
#
# **It was not fixed by making this recipe install**, and the reason is the two
# recipes directly below. `py-mp-bench` and `py-vs-tf2` call it and then unpack
# `crates/tf_tree_py/target/wheels/transform_tree-*-cp314-*.whl` by hand into a
# container. Replacing `maturin build` with `maturin develop` leaves that path
# unwritten and breaks both; doing both would mutate the host's `.venv` as a
# side effect of running a benchmark inside a container, which is a worse
# surprise than the one being fixed. One recipe, one artifact.

# Build a release wheel (it does not install; `just quickstart` does that).
py-wheel:
    # **Clear the previous build first, because the two recipes below index
    # into a glob.** Measured while writing this: after the release bumped the
    # version, `crates/tf_tree_py/target/wheels/` held `tf_tree-0.1.0-…whl`
    # *and* `tf_tree-0.0.1-…whl`, and `glob.glob(…)[0]` is filesystem order,
    # not sorted — so which build those benchmarks measured was luck. It
    # happened to pick the new one on the machine this was found on, which is
    # the worst kind of passing. `target/` is where disposable artifacts live
    # and `cargo clean` removes the directory outright, so nothing is lost that
    # a rebuild does not restore. Both consumers now `assert len(w) == 1` and
    # print what they unpacked, so the invariant this line creates is checked
    # where it is relied on rather than assumed.
    rm -f crates/tf_tree_py/target/wheels/transform_tree-*.whl
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
    # The unpack asserts a single wheel instead of taking `glob(...)[0]`; see
    # `py-wheel` for the stale-wheel it was measured against. A second wheel
    # means somebody built one by hand, and stopping beats benchmarking
    # whichever build the filesystem happens to return first.
    ./docker/tf2/run.sh 'set -e; \
        rm -rf target/pywheel && mkdir -p target/pywheel; \
        python3 -c "import zipfile,glob; w=sorted(glob.glob(\"crates/tf_tree_py/target/wheels/transform_tree-*-cp314-*.whl\")); assert len(w)==1, w; print(\"unpacking\", w[0]); zipfile.ZipFile(w[0]).extractall(\"target/pywheel\")"; \
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
        python3 -c "import zipfile,glob; w=sorted(glob.glob(\"crates/tf_tree_py/target/wheels/transform_tree-*-cp314-*.whl\")); assert len(w)==1, w; print(\"unpacking\", w[0]); zipfile.ZipFile(w[0]).extractall(\"target/pywheel\")"; \
        PYTHONPATH=target/pywheel:$PYTHONPATH python3 crates/tf_tree_bench/python/tf2_ros_compare.py'

# Build, verify and package the CLI for one target — `docs/PHASE5.md` §10's
# "release automation: `cargo-dist` or equivalent".
#
# **Why a recipe and not twenty lines of YAML.** `release.yml` calls this once
# per matrix row, which is the rule `CLAUDE.md` states for every other gate: CI
# invokes the recipe rather than transcribing it, because a transcription
# drifts. It also makes the artifact reproducible on a developer's machine
# without pushing a tag, which is the only way to debug a packaging change.
# `release.yml`'s `pull_request` trigger lists `justfile` for the same reason —
# it did not at first, so an edit to *this recipe* triggered nothing and would
# have been first exercised on a tag.
#
# **Why not `cargo-dist` itself**, which §10 names first: it *generates* the
# workflow from its own config and regenerates it on upgrade. Every other job in
# this repository's workflows carries the argument for why it is shaped the way
# it is, and a generated file cannot. "Or equivalent" is what this is.
#
# **The binary is executed before it is packaged, and this is the whole point of
# the recipe.** A build that emits a file proves the linker ran, not that the
# artifact works: a wrong-architecture cross-build, a truncated write and a
# stale binary from a previous version all produce a plausible-looking file.
# `--version` against the workspace number rejects all three.
GLIBC_FLOOR := "2.34"

release-archive TARGET:
    #!/usr/bin/env bash
    set -euo pipefail
    target="{{ TARGET }}"
    # **`${CARGO_TARGET_DIR:-target}`, not `target/`.** Two gates in this
    # repository (`bench-check`, `c-header-check`) hard-code `./target/` and are
    # silently disabled by a developer who exports `CARGO_TARGET_DIR`; this was
    # the third and is not.
    out_dir="${CARGO_TARGET_DIR:-target}"
    # `cargo pkgid` rather than a third hand-copied `tomllib` one-liner — the
    # other two are `release.yml`'s tag check and `scripts/artifact-versions.py`.
    # It also means this recipe needs no Python at all, which removes an
    # `actions/setup-python` step from all four matrix rows.
    version="$(cargo pkgid -p tf_tree_cli | sed 's/.*[@#]//')"
    name="tf_tree-v${version}-${target}"
    staging="${out_dir}/release-staging"
    stage="${staging}/${name}"

    # **`uname -m` against the triple, before anything is built.** The claim this
    # recipe rests on is that running `--version` proves the artifact is native,
    # and a registered `binfmt_misc`/qemu handler — ordinary on a developer box
    # and in Docker-enabled CI — silently voids it: a cross-built aarch64 binary
    # executes under emulation, answers `--version` correctly, and gets packaged
    # having been verified on the wrong architecture.
    host_arch="$(uname -m)"
    want_arch="${target%%-*}"
    if [ "${host_arch}" != "${want_arch}" ]; then
        echo "::error::this host is ${host_arch}; ${target} needs a ${want_arch} runner." >&2
        echo "  This recipe verifies the artifact by running it, so an emulated" >&2
        echo "  execution would certify a binary nothing native has checked." >&2
        exit 1
    fi

    # `--features shm` and not the default set. `--attach`, `tf_tree top` and
    # `tf_tree participants` are the subcommands somebody reaches for a prebuilt
    # binary to run — they inspect a robot that is already running — and all
    # three are behind that feature. `counters` and `compression` stay on as
    # defaults; `compression` is why an ordinary rosbag2/Foxglove zstd recording
    # opens at all.
    #
    # **`--bin tf_tree`, not the whole package.** `tf_tree_cli` declares two
    # `[[bin]]`s and `tft` is four lines calling the same entry point. Without
    # this, every row pays a second full `lto = "thin"`, `codegen-units = 1` link
    # of a 2.8 MB binary that the staging step below then discards in favour of a
    # symlink.
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

    # **The glibc floor is gated, not merely printed.** It decides whether a
    # `-gnu` archive runs on ROS 2 Humble (Ubuntu 22.04, glibc 2.35), and that
    # number is quoted as a constant in `release.yml`'s release notes, in
    # `README.md` and in `docs/PHASE5.md` §10 — none of which anything checks. A
    # toolchain or dependency bump that raises it would otherwise leave three
    # documents telling a Humble user to download a binary that cannot start.
    # Measured `GLIBC_2.34` on this host and on both `ubuntu-latest` and
    # `ubuntu-24.04-arm`; raising it is a documentation change, so this fails
    # until the prose is updated with it.
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

    # **The whole staging directory, not just this target's subdirectory.**
    # `release.yml` uploads `release-staging/*.tar.gz` by wildcard, and an
    # archive from a previous run — a different target locally, or a `target/`
    # restored by `Swatinem/rust-cache` in CI — is matched by that glob and
    # uploaded as a release asset for a version it does not belong to. The
    # `github-release` job's count check would then fail the release *after* the
    # irreversible crates.io publish.
    rm -rf "${staging}"
    mkdir -p "${stage}"
    cp "${bin}" "${stage}/tf_tree"
    # `tft` is a symlink, not a second binary. `src/bin/tft.rs` is four lines
    # calling the same `tf_tree_cli::run()` and inspects no `argv[0]`, so the two
    # are behaviourally identical — measured, the pair costs 2.27 MB compressed
    # against 1.14 MB for one.
    ln -s tf_tree "${stage}/tft"
    # Apache-2.0 §4(a) and the MIT licence both require the licence text to
    # travel with a binary distribution, exactly as `release.yml` already asserts
    # for the crates.io tarballs. `-L` because these are symlinks inside every
    # crate directory; the copies at the repository root are the real files.
    cp -L LICENSE-MIT LICENSE-APACHE NOTICE README.md "${stage}/"
    for f in LICENSE-MIT LICENSE-APACHE; do
        bytes=$(wc -c < "${stage}/${f}")
        [ "${bytes}" -ge 1000 ] || { echo "::error::${f} is ${bytes} bytes" >&2; exit 1; }
    done
    # **Deterministic packaging.** Two runs against one commit produced two
    # different checksums before these flags existed — from tar's per-file
    # mtimes and gzip's embedded timestamp, with a byte-identical binary inside —
    # which makes the published `.sha256` unverifiable by anyone who rebuilds.
    #
    # **`--mode` is here because the builder's umask was in the archive.**
    # `mkdir` and `cp` both take permissions from it, so the same content packed
    # under `umask 002` and `umask 022` produced two different checksums —
    # measured — which is a developer's box against a GitHub runner. Neither the
    # mtime differential below nor the ownership check could see it: `tar tv`'s
    # second field is uid/gid, not the mode. The capital `X` in `u=rwX,go=rX` is
    # what makes one expression right for both kinds of entry: it grants execute
    # only where it is already set or the entry is a directory, so `tf_tree` and
    # the directory come out `0755` and the licences `0644`, from any input
    # mode. Pinning it here rather than `chmod`-ing the staging directory is the
    # stronger of the two: it makes the archive independent of the staged
    # permissions rather than merely normalising them once.
    pack () {
        tar --sort=name --format=gnu \
            --owner=0 --group=0 --numeric-owner \
            --mode='u=rwX,go=rX' \
            --mtime="@$(git log -1 --format=%ct)" \
            -C "${staging}" -cf - "${name}" \
            | gzip -n -9 > "$1"
    }
    pack "${staging}/${name}.tar.gz"

    # **Two checks, and the obvious one does not work.** Packing twice and
    # comparing is the first thing to reach for and it is vacuous: both packs run
    # inside the same second, so a `date +%s` mtime and the timestamp gzip embeds
    # without `-n` are *identical between them*, and the comparison passes on a
    # build with every flag above removed. Measured, not reasoned — it passed.
    #
    # 1. gzip's header carries MTIME in bytes 4..8. Read it and require zero.
    #    **`-n` is not what makes it zero here**: gzip zeroes MTIME for any input
    #    that is not a regular file, so the pipe above already does it —
    #    `printf x | gzip` gives 0 and `gzip -c file` gives a live timestamp. The
    #    flag is belt-and-braces for an edit that packs from a file instead, and
    #    this assertion is what would catch that edit.
    stamp="$(od -An -tu4 -j4 -N4 < "${staging}/${name}.tar.gz" | tr -d ' ')"
    if [ "${stamp}" != "0" ]; then
        echo "::error::gzip header carries MTIME ${stamp}; -n is not in effect" >&2
        exit 1
    fi
    # 2. `--mtime` and the explicit `chmod`s are what make the contents
    #    independent of when and by whom the staging directory was written.
    #    Re-stamp every staged file to a different date *and* widen its mode,
    #    then repack: pinned, the bytes are identical; unpinned, they are not.
    #    This is the differential the same-second comparison could not produce.
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
    # Ownership is pinned by the flags and not differentially tested here: this
    # recipe cannot chown to a second uid without root, so the flag is asserted
    # by reading tar's own listing rather than by varying the input. (It does
    # fire — removing the flags was measured to fail this check.)
    #
    # **What is deliberately not gated: `--sort=name`.** Removing it was measured
    # and the recipe still passed, because readdir returns this staging
    # directory's six entries in a stable order within a run. Catching it needs
    # two different filesystems, which is not something a recipe can arrange.
    # Said here rather than left to look tested.
    owners="$(tar tvzf "${staging}/${name}.tar.gz" | awk '{print $2}' | sort -u)"
    if [ "${owners}" != "0/0" ]; then
        echo "::error::archive records ownership '${owners}', expected 0/0" >&2
        exit 1
    fi

    # Unpack what was just packed and run *that*, through the symlink. The
    # archive is the artifact a user receives, and nothing above has yet proven
    # the thing inside it survives a round trip — a dereferenced or dangling
    # `tft`, or a tar that recorded the staging path rather than the binary,
    # both pack without complaint.
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

# The CycloneDX SBOM `docs/PHASE5.md` §10 asks for, per release.
#
# Written from `cargo metadata` rather than by adding `cargo-cyclonedx`, for the
# reason `release.yml` already gives for not using `cargo-dist`: a generated
# artifact cannot carry the argument for its own shape. The script's docstring
# carries this one — in particular why the graph is walked from the *shipped*
# roots over `normal` edges only, so a dev-dependency never appears in a bill of
# materials for something that does not contain it.
sbom VERSION:
    python3 scripts/sbom.py --version {{ VERSION }} -o target/tf_tree-{{ VERSION }}-sbom.cdx.json

# `docs/PHASE2.md` §11.2 scenario 9, a thousand times — §15's box 6.
#
# §11.2 calls the split-brain race "the single most important race in the
# phase" and asks for it in a loop of a thousand. That does not belong in
# `just test`: one run is the regression gate, and a thousand is a soak whose
# value is the *tail* — the one interleaving in a few hundred where a newcomer
# arrives inside the window between the owner's death and a survivor noticing.
#
# The child's open timeout is an argument for this recipe's sake: the ownerless
# arm waits the timeout out before refusing, so at the 5 s default a thousand
# runs would be eighty-three minutes of sleeping rather than of racing.
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
