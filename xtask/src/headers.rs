//! `cargo xtask headers [--check]` — generate `tf_tree.h` and
//! `tf_tree_unstable.h` from `tf_tree_c`.
//!
//! # Why this is an xtask and not a `build.rs`
//!
//! `docs/decisions/0007` decides it: `cbindgen` is MPL-2.0 and `deny.toml`'s
//! licence allowlist has no MPL entry, so it cannot be a dependency of anything
//! the workspace builds. Invoking the **binary** keeps it out of the dependency
//! graph entirely, and a developer who does not have it installed simply cannot
//! regenerate headers — which is fine, because the headers are committed.
//!
//! # Why the headers are committed
//!
//! §3.1: the stable header is the first ABI freeze in the project and its
//! surface is permanent. A generated-at-build-time header makes an ABI change
//! invisible in review; a committed one makes it a diff somebody has to approve.
//! `--check` is what keeps the two honest, and CI runs it.
//!
//! # The partition is a list, on purpose
//!
//! §3.1 splits the surface in two: `tf_tree.h` is semver-frozen, and
//! `tf_tree_unstable.h` promises nothing and needs `#define TFT_ENABLE_UNSTABLE`.
//! Every exported symbol must appear in exactly one of [`STABLE`], [`UNSTABLE`]
//! or [`TEST_ONLY`] below, and [`check_partition`] fails if the source and the
//! lists disagree in *either* direction.
//!
//! That means **a newly added `extern "C"` function fails this check until
//! somebody puts it in a tier.** Defaulting new symbols into the stable header
//! would be the wrong default for a surface that can never be withdrawn, and
//! defaulting them into the unstable one would let the stable header silently
//! stop describing the library. Neither is a decision a tool should make.
//!
//! **[`check_partition`] scans `extern "C" fn` declarations only**, so a new
//! `pub const` is not caught by it. It is still not free-floating: both configs
//! are built from these lists by *complement*, so a constant missing from
//! [`STABLE`] lands in **both** headers rather than just the stable one — and
//! `--check` then reports the unstable header as drifted. That is how the two
//! codes added after review were caught. The list is therefore an inventory of
//! the frozen surface, not merely documentation of one.
//!
//! # Why [`check_overlap`] exists as well
//!
//! The paragraph above is true only while the *committed* headers are the ones
//! from before the omission. `TFT_ERR_ARENA_UNAVAILABLE` was added by
//! `docs/decisions/0015`, left out of [`STABLE`], and regenerated — so both
//! committed headers grew it in the same commit, `--check` saw no drift at all,
//! and `just c-header-check`'s two-compiler matrix saw nothing either: an
//! **identical** `#define` twice is legal C, and `tf_tree_unstable.h` includes
//! `tf_tree.h`. The symbol shipped in both headers with a 28-line doc block
//! duplicated behind it, and every gate passed.
//!
//! §3.1's two-tier split *is* the stability promise, so a partition that only
//! holds by accident is not a promise. [`check_overlap`] reads the two generated
//! headers back and fails if any symbol is **defined** in both — which is the
//! omission's signature, whatever kind of item it is, and unlike
//! [`check_partition`] it needs no list of item kinds to know about.
//!
//! It reads *definitions*, never references: `tf_tree_unstable.h` legitimately
//! mentions `tft_status`, `tft_tree` and half the stable header in its own
//! declarations, and including it in `tf_tree.h` is exactly how that is meant to
//! work.
//!
//! # And [`check_stable_is_complete`] for the opposite sign
//!
//! `check_overlap` catches a symbol in *both* headers. The same omission with
//! the opposite sign — a [`STABLE`] entry in **neither** — makes the frozen
//! header quietly *smaller*, and until it was added nothing caught that at all.
//!
//! Measured, by making `TFT_ERR_ARENA_UNAVAILABLE` `pub(crate)` and
//! regenerating in the same run, which is exactly how the overlap defect shipped:
//!
//! ```text
//! $ cargo run -p xtask -- headers        # regenerate, as the guilty commit did
//! $ cargo run -p xtask -- headers --check
//! xtask headers: both headers are up to date
//! exit=0
//! $ grep -c '#define TFT_ERR_ARENA_UNAVAILABLE' include/*.h
//! 0
//! ```
//!
//! Every gate green with the constant defined nowhere. `--check` compares each
//! header against a copy regenerated with the same omission; `check_overlap`
//! sees no intersection because there is nothing in either file to intersect;
//! and [`check_partition`] filters entries without a lowercase character, so
//! every screaming-case constant is exempt from its stale half by construction.
//! The three doc-comment *references* to the code survive in both headers, so
//! the frozen header is left documenting a status a caller cannot name.
//!
//! §3.1 calls the frozen header a promise that can never be withdrawn. This is
//! the check that stops it being withdrawn by deleting a line.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// The frozen surface — `tf_tree.h`. **Semver from 1.0; nothing leaves.**
const STABLE: &[&str] = &[
    // Version and compatibility — §3.6.
    "TFT_ABI_VERSION_MAJOR",
    "TFT_ABI_VERSION_MINOR",
    "tft_abi_version_major",
    "tft_abi_version_minor",
    "tft_check_abi",
    // Errors — §3.3.
    "tft_status",
    "tft_error",
    "tft_last_error",
    "TFT_MESSAGE_LEN",
    "TFT_INVALID_ID",
    "TFT_OK",
    "TFT_ERR_NULL_ARG",
    "TFT_ERR_BAD_HANDLE",
    "TFT_ERR_BAD_STRUCT_SIZE",
    "TFT_ERR_BAD_ENUM",
    "TFT_ERR_BUFFER_TOO_SMALL",
    "TFT_ERR_ABI_MISMATCH",
    "TFT_ERR_NOT_FINITE",
    "TFT_ERR_NOT_A_ROTATION",
    "TFT_ERR_UNKNOWN_FRAME",
    "TFT_ERR_DISCONNECTED",
    "TFT_ERR_NO_DATA",
    "TFT_ERR_EXTRAPOLATION",
    "TFT_ERR_TOPOLOGY_CHANGED",
    "TFT_ERR_TIME_DOMAIN",
    "TFT_ERR_SLOT_RECYCLED",
    "TFT_ERR_SLOT_CONTENDED",
    "TFT_ERR_CHILD_DETACHED",
    "TFT_ERR_NO_DERIVATIVES",
    "TFT_ERR_NO_SEGMENT",
    "TFT_ERR_TREE_TOO_DEEP",
    "TFT_ERR_WRONG_THREAD",
    "TFT_ERR_ALREADY_CLAIMED",
    "TFT_ERR_NON_MONOTONIC",
    "TFT_ERR_CLAIM_REVOKED",
    "TFT_ERR_NOT_DYNAMIC",
    "TFT_ERR_READ_ONLY",
    "TFT_ERR_RETRY",
    "TFT_ERR_RELEASED",
    "TFT_ERR_PARENT_MISMATCH",
    "TFT_ERR_NO_EDGE",
    // Returned only by `tft_bridge_create` today, and still stable: §3.3 defines
    // one status space, the constant is not feature-gated, and the frozen header
    // is where a C programmer looks up a negative number they were handed. A
    // code that lived in the unstable header would be un-nameable by a caller
    // that never opted into `TFT_ENABLE_UNSTABLE` but can still receive it once
    // any stable entry point starts parsing config.
    "TFT_ERR_BAD_CONFIG",
    // Same argument, one code over, and it is here because it was **missing**:
    // added by `docs/decisions/0015` and left out of this list, so the
    // complement below emitted it into the unstable header as well and both
    // committed headers carried it. That was legal C — an identical `#define`
    // twice — so the compile matrix passed. [`check_overlap`] is the check that
    // now fails on it.
    "TFT_ERR_ARENA_UNAVAILABLE",
    // Returned only by the two stamp converters below, and stable for the same
    // reason every other status code is: the library hands it *back*, so a
    // caller who never defines `TFT_ENABLE_UNSTABLE` must still be able to name
    // the negative number it received.
    "TFT_ERR_BAD_STAMP",
    "TFT_ERR_INTERNAL",
    // Layouts — §3.5.
    "tft_layout",
    "TFT_LAYOUT_QVEC7_WXYZ",
    "TFT_LAYOUT_QVEC7_XYZW",
    "TFT_LAYOUT_MAT4_COL",
    "TFT_LAYOUT_MAT4_ROW",
    "TFT_LAYOUT_AFFINE12_ROW_F32",
    // Appended by `docs/API.md` §3.3 — a minor bump under §3.6. It is stable
    // for one reason and it is not `TFT_ERR_BAD_CONFIG`'s: **`tft_plan_at` and
    // `tft_plan_at_many` accept it**, and both are in this list. A value a
    // frozen entry point takes has to be spellable by a caller who never
    // defines `TFT_ENABLE_UNSTABLE`, or that entry point's signature is a lie
    // about what it accepts.
    //
    // The symmetry with `TFT_ERR_BAD_CONFIG` above does *not* hold and an
    // earlier revision of this comment claimed it did: a status code is stable
    // because the library hands it *back* to a caller who must be able to name
    // it, while `tft_layout` is a pure input — it appears in this header only
    // as a parameter and is never a return value or out-param. A caller who
    // cannot name it can never receive it.
    "TFT_LAYOUT_QVEC7_WXYZ_TWIST6",
    "tft_layout_size",
    // Stamps — `docs/API.md` §5.1. Pure functions over integers: no handle, no
    // arena, nothing to get wrong at a lifetime. They are in the frozen header
    // because the conversion they replace is the one every ROS 2 node writes by
    // hand, and a caller who has to opt into an unstable header to avoid
    // writing it will write it.
    "tft_stamp_from_parts",
    "tft_stamp_from_timespec",
    // Lifecycle and the hot path — §3.2, §3.7.
    "tft_tree_open",
    "tft_tree_free",
    "tft_plan_create",
    "tft_plan_free",
    "tft_plan_at",
    "tft_plan_at_many",
    // Publishing — §3.2.
    "tft_tree_claim",
    "tft_publisher_push",
    "tft_publisher_push_many",
    "tft_publisher_release",
    "tft_publisher_free",
];

/// The unstable surface — `tf_tree_unstable.h`. **No guarantee of any kind.**
const UNSTABLE: &[&str] = &[
    "TFT_TWIST_BYTES",
    "tft_plan_at_with_derivatives",
    "tft_tree_frame_count",
    "tft_tree_edge_count",
    "tft_tree_frame_name",
    "tft_tree_instance_uuid",
    // The ROS 2 ingest-bridge seam — `docs/PHASE4.md` §5, compiled only under
    // `--features bridge` and therefore emitted inside `#if
    // defined(TFT_HAVE_BRIDGE)`. **Unstable on purpose**: §5 is the half of
    // Phase 4 that a year of dogfooding is expected to argue with, and freezing
    // a ROS-shaped surface before the argument has happened is how a
    // compatibility promise becomes a liability.
    "tft_bridge_create",
    "tft_bridge_tree",
    "tft_bridge_free",
    "tft_bridge_offer",
    "tft_bridge_attribute",
    "tft_bridge_note_message",
    "tft_bridge_note_time_jump",
    "tft_bridge_note_queue_depth",
    "tft_bridge_get_stats",
    "tft_bridge_get_remap",
    "tft_bridge_topic",
    "tft_bridge_jump_kind",
    "tft_bridge_evidence",
    "tft_bridge_authority",
    "tft_bridge_on_clock_reset",
    "tft_bridge_action",
    "tft_bridge_reason",
    "tft_bridge_sample",
    "tft_bridge_outcome",
    "tft_bridge_options",
    "tft_bridge_stats",
    "tft_bridge_remap",
    "TFT_BRIDGE_TOPIC_TF",
    "TFT_BRIDGE_TOPIC_TF_STATIC",
    "TFT_BRIDGE_AUTHORITY_FIRST_WRITER_WINS",
    "TFT_BRIDGE_AUTHORITY_LAST_WRITER_WINS",
    "TFT_BRIDGE_AUTHORITY_STRICT",
    "TFT_BRIDGE_ON_CLOCK_RESET_HALT",
    "TFT_BRIDGE_ON_CLOCK_RESET_RECREATE",
    "TFT_BRIDGE_APPLIED",
    "TFT_BRIDGE_STATIC_VERIFIED",
    "TFT_BRIDGE_DROPPED",
    "TFT_BRIDGE_UNDECLARED",
    "TFT_BRIDGE_STATIC_CONFLICT",
    "TFT_BRIDGE_HALT",
    "TFT_BRIDGE_RECREATE",
    "TFT_BRIDGE_REJECTED",
    "TFT_BRIDGE_REASON_NONE",
    "TFT_BRIDGE_REASON_BAD_NAME",
    "TFT_BRIDGE_REASON_NOT_THE_OWNER",
    "TFT_BRIDGE_REASON_NON_MONOTONIC",
    "TFT_BRIDGE_REASON_KIND_CHANGE",
    "TFT_BRIDGE_REASON_AUTHORITY_CONFLICT",
    "TFT_BRIDGE_REASON_CLOCK_RESET",
    "TFT_BRIDGE_REASON_BAD_POSE",
    "TFT_BRIDGE_REASON_ALREADY_HALTED",
    "TFT_BRIDGE_JUMP_CLOCK_TYPE_CHANGED",
    "TFT_BRIDGE_JUMP_BACKWARD",
    "TFT_BRIDGE_JUMP_FORWARD",
    "TFT_BRIDGE_EVIDENCE_NONE",
    "TFT_BRIDGE_EVIDENCE_REPORTED",
    "TFT_BRIDGE_EVIDENCE_COMMON_MODE",
];

/// Tier entries that name a **type**, not an `extern "C" fn`.
///
/// They are in the lists to steer `cbindgen`'s exclude-by-complement, so
/// [`check_partition`]'s reverse direction — "this entry names no exported
/// function" — must skip them. Constants are skipped by the `is_lowercase`
/// filter; these are not, because C type names here are lowercase by
/// convention.
const TIER_TYPES: &[&str] = &[
    "tft_status",
    "tft_error",
    "tft_layout",
    "tft_bridge_topic",
    "tft_bridge_authority",
    "tft_bridge_on_clock_reset",
    "tft_bridge_action",
    "tft_bridge_reason",
    "tft_bridge_jump_kind",
    "tft_bridge_evidence",
    "tft_bridge_sample",
    "tft_bridge_outcome",
    "tft_bridge_options",
    "tft_bridge_stats",
    "tft_bridge_remap",
];

/// Rust types `cbindgen` must never emit: the opaque handles (§3.2) and the
/// private types it would otherwise forward-declare in order to spell their
/// fields.
const OPAQUE: &[&str] = &[
    "tft_tree",
    "tft_plan",
    "tft_publisher",
    "tft_bridge",
    "Arc_TreeShare",
    "Option_EdgeWriter",
    "Box_BridgeInner",
];

/// Compiled only under `--features test-hooks`; never in a shipped header.
const TEST_ONLY: &[&str] = &[
    "tft_test_tree_create",
    "tft_test_publishable_tree_create",
    "tft_test_lerpslerp_tree_create",
    "tft_test_panic",
    "tft_guarded_noop",
    "tft_test_push_unguarded",
];

/// The handle types, declared by hand rather than generated.
///
/// §3.2 requires them opaque. `cbindgen`'s `cbindgen:opaque` annotation does not
/// take effect on `#[repr(C)]` structs whose fields it cannot name, so it emits
/// the layout — `Arc_TreeShare share;` and all — which a C caller could then
/// dereference. Excluding them and writing the forward declarations here is both
/// correct and what §3.1 asks for anyway ("frozen and reviewed by hand, not
/// merely `cbindgen` output").
const HANDLE_DECLS: &str = "\
/*
 * Opaque handles — docs/PHASE4.md §3.2.
 *
 *   tft_tree       Send + Sync   shareable across threads
 *   tft_plan       Send + Sync   shareable, immutable
 *   tft_publisher  Send + !Sync  ONE THREAD AT A TIME
 *
 * The publisher's thread affinity is not advisory: a debug build of the library
 * abort()s if you use one from a thread other than the one that claimed it, and
 * a release build returns TFT_ERR_WRONG_THREAD.
 */
typedef struct tft_tree tft_tree;
typedef struct tft_plan tft_plan;
typedef struct tft_publisher tft_publisher;
";

/// The bridge handle, declared by hand for the same reason as [`HANDLE_DECLS`]
/// and guarded because the symbol only exists under `--features bridge`.
///
/// It lives in the *unstable* header: §5's shape is what a year of dogfooding is
/// meant to argue with, so nothing about it belongs in the frozen one.
const BRIDGE_DECLS: &str = "\
#if defined(TFT_HAVE_BRIDGE)
/*
 * The ingest bridge — docs/PHASE4.md §5.
 *
 *   tft_bridge  Send + !Sync   ONE THREAD AT A TIME
 *
 * Same affinity rule, and for a sharper reason than tft_publisher's: the handle
 * holds one claim per declared dynamic edge, so using it from a second thread
 * would write the arena from a thread that does not own those claims. §5.9 asks
 * for a dedicated SingleThreadedExecutor on its own thread, which is exactly the
 * shape this allows.
 *
 * Every const char * in tft_bridge_outcome is borrowed from the handle and
 * valid only until the next call on it. None is ever NULL; a field that does not
 * apply to an outcome is the empty string.
 */
typedef struct tft_bridge tft_bridge;
#endif  /* TFT_HAVE_BRIDGE */
";

const GENERATED_BANNER: &str = "\
/*
 * GENERATED FILE — do not edit.
 *
 * Regenerate with `cargo xtask headers`; `cargo xtask headers --check` fails if
 * this file and crates/tf_tree_c/src/ have drifted. The file is committed on
 * purpose (docs/decisions/0007): an ABI change should be a diff somebody
 * approves, not something that materialises during a build.
 */
";

const UNSTABLE_PREAMBLE: &str = "\
/*
 * tf_tree — the UNSTABLE C API.  docs/PHASE4.md §3.1.
 *
 * NOTHING HERE IS COVERED BY ANY COMPATIBILITY PROMISE.  A symbol in this
 * header may change signature, change meaning, or disappear in a patch
 * release.  It exists so that work which needs derivatives or introspection
 * today is not blocked on freezing an interface a year of use has not yet
 * argued with.
 *
 * You must #define TFT_ENABLE_UNSTABLE before including this file.  That is a
 * speed bump, deliberately: it means nobody reaches these symbols by accident
 * and then reports their removal as a regression.
 */
#ifndef TFT_ENABLE_UNSTABLE
#error \"tf_tree_unstable.h has no stability guarantee; #define TFT_ENABLE_UNSTABLE to accept that\"
#endif

#include \"tf_tree.h\"
";

pub(crate) fn run(check: bool) -> ExitCode {
    let root = workspace_root();
    let crate_dir = root.join("crates/tf_tree_c");
    let include_dir = crate_dir.join("include");

    if let Err(e) = check_partition(&crate_dir) {
        eprintln!("xtask headers: {e}");
        return ExitCode::FAILURE;
    }

    let stable = match generate(&crate_dir, Tier::Stable) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("xtask headers: {e}");
            return ExitCode::FAILURE;
        }
    };
    let unstable = match generate(&crate_dir, Tier::Unstable) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("xtask headers: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Before either header is written or compared. A `--check` run that only
    // diffed would pass on a duplicate that was committed together with the
    // omission that produced it, which is precisely what happened.
    if let Err(e) = check_overlap(&stable, &unstable) {
        eprintln!("xtask headers: {e}");
        return ExitCode::FAILURE;
    }

    // The other direction, and the one [`check_overlap`] cannot see: a symbol
    // in **neither** header.
    if let Err(e) = check_stable_is_complete(&stable) {
        eprintln!("xtask headers: {e}");
        return ExitCode::FAILURE;
    }

    let targets = [
        (include_dir.join("tf_tree.h"), stable),
        (include_dir.join("tf_tree_unstable.h"), unstable),
    ];

    let mut drifted = false;
    for (path, want) in &targets {
        if check {
            let got = std::fs::read_to_string(path).unwrap_or_default();
            if &got != want {
                drifted = true;
                eprintln!(
                    "xtask headers: {} is out of date.\n  \
                     The committed header and crates/tf_tree_c/src/ disagree — which means the\n  \
                     ABI changed without the header change being reviewed. Run\n  \
                     `cargo xtask headers` and include the result in the same commit.",
                    path.display()
                );
                report_first_difference(&got, want);
            }
        } else {
            if let Some(parent) = path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!("xtask headers: cannot create {}: {e}", parent.display());
                    return ExitCode::FAILURE;
                }
            }
            if let Err(e) = std::fs::write(path, want) {
                eprintln!("xtask headers: cannot write {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
            println!("xtask headers: wrote {}", path.display());
        }
    }

    if drifted {
        ExitCode::FAILURE
    } else {
        if check {
            println!("xtask headers: both headers are up to date");
        }
        ExitCode::SUCCESS
    }
}

/// Print the first differing line, so a `--check` failure in CI says *what*
/// changed rather than only that something did.
fn report_first_difference(got: &str, want: &str) {
    for (i, (g, w)) in got.lines().zip(want.lines()).enumerate() {
        if g != w {
            eprintln!("  first difference at line {}:", i + 1);
            eprintln!("    committed: {g}");
            eprintln!("    generated: {w}");
            return;
        }
    }
    let (a, b) = (got.lines().count(), want.lines().count());
    if a != b {
        eprintln!("  the committed header has {a} lines; the generated one has {b}");
    }
}

#[derive(Clone, Copy)]
enum Tier {
    Stable,
    Unstable,
}

fn generate(crate_dir: &Path, tier: Tier) -> Result<String, String> {
    let cfg_path = std::env::temp_dir().join(match tier {
        Tier::Stable => "tf_tree_cbindgen_stable.toml",
        Tier::Unstable => "tf_tree_cbindgen_unstable.toml",
    });
    std::fs::write(&cfg_path, config_for(tier)).map_err(|e| format!("writing config: {e}"))?;

    let out = Command::new("cbindgen")
        .args([
            "--config",
            &cfg_path.to_string_lossy(),
            "--crate",
            "tf_tree_c",
        ])
        .current_dir(crate_dir)
        .output()
        .map_err(|e| {
            format!(
                "could not run `cbindgen`: {e}\n  \
                 Install it with `cargo install cbindgen`. It is deliberately not a\n  \
                 workspace dependency (docs/decisions/0007: MPL-2.0 against deny.toml's\n  \
                 allowlist), and the headers are committed so this is only needed when\n  \
                 changing the ABI."
            )
        })?;
    if !out.status.success() {
        return Err(format!(
            "cbindgen failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let body = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(assemble(tier, &body))
}

fn config_for(tier: Tier) -> String {
    // **cbindgen emits declarations only; every wrapper comes from `assemble`.**
    //
    // `no_includes` suppresses the `#include`s, no `include_guard` key
    // suppresses the guard, and `cpp_compat = false` suppresses the
    // `extern "C"` block. The alternative — letting cbindgen emit them and
    // filtering afterwards — was tried and shipped a header that would not
    // compile: the filter dropped `#ifdef __cplusplus` and `extern "C" {` but
    // left the matching `#endif  // __cplusplus`, so gcc, clang, g++ and
    // clang++ all reported `#endif without #if`. Generating nothing is easier
    // to get right than un-generating something.
    let _ = tier;
    let mut s = String::from(
        "language = \"C\"\n\
         style = \"type\"\n\
         cpp_compat = false\n\
         no_includes = true\n\
         documentation_style = \"doxy\"\n\
         usize_is_size_t = true\n\
         \n\
         [parse]\n\
         parse_deps = false\n\
         \n\
         # `tft_tree_open` only exists under `--features shm`. Without this the\n\
         # header would declare a symbol the default build does not export, and\n\
         # the caller would find out at link time.\n\
         [defines]\n\
         \"feature = shm\" = \"TFT_HAVE_SHM\"\n\
         # Likewise for the ingest bridge (§5), which is default-off. Without\n\
         # this the unstable header would declare eight symbols an ordinary\n\
         # build does not export.\n\
         \"feature = bridge\" = \"TFT_HAVE_BRIDGE\"\n\
         \n",
    );
    match tier {
        Tier::Stable => {
            s.push_str("[export]\nexclude = [");
            // Everything that is not stable, plus the hand-declared handles and
            // the private types `cbindgen` would otherwise forward-declare in
            // order to spell their fields.
            let excluded: Vec<&str> = UNSTABLE
                .iter()
                .chain(TEST_ONLY)
                .copied()
                .chain(OPAQUE.iter().copied())
                .collect();
            push_list(&mut s, &excluded);
        }
        Tier::Unstable => {
            // `exclude`, not `include`. cbindgen's `[export] include` is
            // **additive** — "always emit these" — not restrictive, so an
            // `include`-only config emitted the entire surface into the
            // unstable header (856 lines for six symbols). The complement is
            // the only way to say "just these".
            s.push_str("[export]\nexclude = [");
            let excluded: Vec<&str> = STABLE
                .iter()
                .chain(TEST_ONLY)
                .copied()
                .chain(OPAQUE.iter().copied())
                .collect();
            push_list(&mut s, &excluded);
        }
    }
    s.push_str("]\n");
    s
}

fn push_list(s: &mut String, items: &[&str]) {
    for (i, name) in items.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('"');
        s.push_str(name);
        s.push('"');
    }
}

fn assemble(tier: Tier, body: &str) -> String {
    let mut out = String::new();
    out.push_str(GENERATED_BANNER);
    out.push('\n');
    match tier {
        Tier::Stable => {
            out.push_str(
                "/*\n * tf_tree — the stable C API.  docs/PHASE4.md §3.\n *\n \
                 * Every function returns tft_status: 0 on success, negative on failure.\n \
                 * On failure, tft_last_error() fills a tft_error with structured detail\n \
                 * for THIS THREAD, valid until the next tf_tree call on this thread.\n \
                 * That thread-local lifetime is the single most common C-API misuse, so\n \
                 * it is stated here and not only in the manual.\n *\n \
                 * No entry point can abort your process: every one wraps its body in a\n \
                 * panic guard (§3.4), so an internal bug becomes TFT_ERR_INTERNAL.\n */\n",
            );
            out.push_str("#ifndef TF_TREE_H\n#define TF_TREE_H\n\n");
            out.push_str("#include <stdint.h>\n#include <stddef.h>\n\n");
            out.push_str("#ifdef __cplusplus\nextern \"C\" {\n#endif\n\n");
            out.push_str(HANDLE_DECLS);
            out.push('\n');
            out.push_str(body.trim_start_matches('\n'));
            out.push_str("\n#ifdef __cplusplus\n}  /* extern \"C\" */\n#endif\n\n");
            out.push_str("#endif  /* TF_TREE_H */\n");
        }
        Tier::Unstable => {
            out.push_str(UNSTABLE_PREAMBLE);
            out.push_str("\n#ifndef TF_TREE_UNSTABLE_H\n#define TF_TREE_UNSTABLE_H\n\n");
            out.push_str("#ifdef __cplusplus\nextern \"C\" {\n#endif\n\n");
            out.push_str(BRIDGE_DECLS);
            out.push('\n');
            out.push_str(body.trim_start_matches('\n'));
            out.push_str("\n#ifdef __cplusplus\n}  /* extern \"C\" */\n#endif\n\n");
            out.push_str("#endif  /* TF_TREE_UNSTABLE_H */\n");
        }
    }
    out
}

/// Every `extern "C"` symbol in `tf_tree_c` must be in exactly one tier.
///
/// Checked in **both** directions: a new function that nobody classified fails,
/// and so does a stale list entry naming a function that has been removed. The
/// first is the one that matters — see the module docs for why a tool must not
/// pick a tier on its own.
fn check_partition(crate_dir: &Path) -> Result<(), String> {
    let mut found = BTreeSet::new();
    let src = crate_dir.join("src");
    let entries = std::fs::read_dir(&src).map_err(|e| format!("reading {}: {e}", src.display()))?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        for line in text.lines() {
            let t = line.trim();
            // `pub extern "C" fn NAME(` / `pub unsafe extern "C" fn NAME(`.
            let Some(rest) = t
                .strip_prefix("pub unsafe extern \"C\" fn ")
                .or_else(|| t.strip_prefix("pub extern \"C\" fn "))
            else {
                continue;
            };
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                found.insert(name);
            }
        }
    }

    let classified: BTreeSet<String> = STABLE
        .iter()
        .chain(UNSTABLE)
        .chain(TEST_ONLY)
        .map(|s| (*s).to_string())
        .collect();

    let unclassified: Vec<&String> = found.difference(&classified).collect();
    if !unclassified.is_empty() {
        return Err(format!(
            "these exported functions are in no tier: {unclassified:?}\n  \
             Add each to STABLE, UNSTABLE or TEST_ONLY in xtask/src/headers.rs.\n  \
             There is deliberately no default: putting a symbol in the frozen header\n  \
             is a promise that can never be withdrawn (§3.1), and leaving it out means\n  \
             the header stops describing the library. Neither is a tool's call."
        ));
    }

    // The reverse direction. Only function names are scanned, so constants and
    // types in the lists are skipped here rather than reported as stale.
    let stale: Vec<&&str> = STABLE
        .iter()
        .chain(UNSTABLE)
        .chain(TEST_ONLY)
        .filter(|n| n.starts_with("tft_") && n.contains(|c: char| c.is_lowercase()))
        .filter(|n| !found.contains(**n))
        // Types, not functions: they are in the lists to steer `cbindgen`'s
        // include/exclude and have no `extern "C" fn` to find.
        .filter(|n| !TIER_TYPES.contains(*n))
        .collect();
    if !stale.is_empty() {
        return Err(format!(
            "these tier entries name no exported function: {stale:?}\n  \
             Either the function was removed (drop the entry) or renamed (update it).\n  \
             A stale entry in STABLE is the dangerous one: it makes the frozen header\n  \
             look larger than the library actually is."
        ));
    }
    Ok(())
}

/// **No symbol may be defined by both generated headers.**
///
/// §3.1's split is the stability promise: a symbol in `tf_tree.h` can never be
/// withdrawn, and one in `tf_tree_unstable.h` carries no promise at all. A
/// symbol in *both* is in neither tier — it is unwithdrawable and unpromised at
/// the same time — and it means the same thing every time: an entry missing from
/// [`STABLE`], which the complement in [`config_for`] then emits into both.
///
/// This is checked on the **generated** text rather than on the committed files
/// because the generated text is the authority; drift between the two is
/// [`run`]'s own comparison, and a duplicate reaches the committed files only
/// through here.
///
/// Nothing else can catch it. `--check` diffs each header against its own
/// committed copy, and a duplicate committed *with* the omission that caused it
/// makes both copies match. `just c-header-check`'s gcc/clang/g++/clang++ matrix
/// compiles a translation unit that includes both, and an identical `#define`
/// twice is legal C — not even under `-Wpedantic -Werror`.
fn check_overlap(stable: &str, unstable: &str) -> Result<(), String> {
    let in_stable = defined_symbols(stable);
    let in_unstable = defined_symbols(unstable);
    let both: Vec<&String> = in_stable.intersection(&in_unstable).collect();
    if both.is_empty() {
        return Ok(());
    }
    Err(format!(
        "these symbols are defined by BOTH generated headers: {both:?}\n  \
         docs/PHASE4.md §3.1 splits the surface in two and that split is the\n  \
         stability promise, so a symbol cannot be in both tiers. This almost always\n  \
         means the symbol is missing from STABLE in xtask/src/headers.rs: both\n  \
         cbindgen configs are built by *complement*, so an unlisted item is excluded\n  \
         from neither and lands in both files.\n  \
         Add it to STABLE (or to UNSTABLE, if the frozen header is not where it\n  \
         belongs) and regenerate."
    ))
}

/// Every [`STABLE`] entry is actually **defined** by the frozen header.
///
/// # The direction [`check_overlap`] cannot see
///
/// `check_overlap` catches a symbol in *both* headers. This catches one in
/// **neither**, which is the same defect with the opposite sign and a worse
/// consequence: the frozen header silently gets **smaller**.
///
/// [`check_partition`] does not cover it. That function scans the Rust source
/// for `extern "C" fn` and its stale-entry half filters on
/// `n.contains(char::is_lowercase)` — so `TFT_ERR_ARENA_UNAVAILABLE`,
/// `TFT_ABI_VERSION_MINOR` and every other screaming-case constant is exempt by
/// construction, and a *type* has no `extern "C" fn` to find at all. Delete a
/// `pub const` from `tf_tree_c` and leave its name in [`STABLE`] and nothing
/// notices: `cbindgen` emits it in neither file (the unstable config excludes it
/// by complement, the stable one has nothing to emit), the two headers stay
/// disjoint so `check_overlap` is silent, `--check` diffs both against copies
/// regenerated with the same omission, and the compile matrix compiles a
/// translation unit that no longer names it.
///
/// **That is a withdrawn stability promise, made by deleting a line.** §3.1 says
/// the frozen header is the promise that can never be withdrawn; this is the
/// check that makes withdrawing it require saying so.
///
/// Grounded in the generated artifact rather than in a second scan of the
/// source, so it covers constants, types and functions with one rule — the same
/// reason the `TFT_HAVE_SHM` probe in `crates/tf_tree_c/CMakeLists.txt` asks the
/// library instead of trusting a list.
fn check_stable_is_complete(stable_header: &str) -> Result<(), String> {
    let defined = defined_symbols(stable_header);
    let missing: Vec<&&str> = STABLE.iter().filter(|n| !defined.contains(**n)).collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "these STABLE entries are defined by NEITHER generated header: {missing:?}\n  \
         STABLE is the frozen tier (docs/PHASE4.md §3.1), and both cbindgen configs\n  \
         are built by *complement* — so a name listed here that the crate no longer\n  \
         exports is emitted nowhere, and the frozen header quietly gets smaller.\n  \
         That is a stability promise withdrawn by deleting a line, which is the one\n  \
         thing §3.1 says cannot happen.\n  \
         Either the item still exists and its name changed (update the entry), or it\n  \
         was removed on purpose — in which case removing it from STABLE is an ABI\n  \
         break and needs the major bump and the record that go with one."
    ))
}

/// Every symbol a header **defines** — not the ones it merely mentions.
///
/// `tf_tree_unstable.h` includes `tf_tree.h` and names stable types all over its
/// own declarations, so a scan that counted references would report the entire
/// stable surface as an overlap. Four definition shapes cover everything the two
/// headers contain, and all four sit at **column 0**: `cbindgen` indents every
/// struct field, every enum variant and every continuation line of a wrapped
/// declaration, so the column is what separates a definition from the inside of
/// one.
fn defined_symbols(header: &str) -> BTreeSet<String> {
    let mut defs = BTreeSet::new();
    for line in strip_comments(header).lines() {
        if line.is_empty() || line.starts_with(|c: char| c.is_whitespace()) {
            continue;
        }
        let t = line.trim_end();
        // 1. `#define NAME ...`
        if let Some(rest) = t.strip_prefix("#define ") {
            let name = ident_prefix(rest.trim_start());
            if !name.is_empty() {
                defs.insert(name);
            }
            continue;
        }
        // Any other preprocessor directive defines nothing.
        if t.starts_with('#') {
            continue;
        }
        // 2. `typedef <...> NAME;` and 3. the `} NAME;` closing a struct or enum
        //    body opened by `typedef struct {`.
        if t.starts_with("typedef") || t.starts_with('}') {
            if let Some(name) = last_ident_before_semicolon(t) {
                defs.insert(name);
            }
            continue;
        }
        // `extern "C" {` and its closing brace; neither declares anything.
        if t.starts_with("extern") {
            continue;
        }
        // 4. A function declaration: the identifier immediately before the first
        //    `(`. Wrapped parameter lists continue on indented lines, which the
        //    column-0 test above has already dropped.
        if let Some(open) = t.find('(') {
            let name = ident_suffix(&t[..open]);
            if !name.is_empty() {
                defs.insert(name);
            }
        }
    }
    defs
}

/// Blank out C comments, preserving both line structure and column positions —
/// [`defined_symbols`] relies on the latter.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut in_block = false;
    for line in src.lines() {
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if in_block {
                if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    in_block = false;
                    out.push_str("  ");
                    i += 2;
                } else {
                    out.push(' ');
                    i += 1;
                }
            } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                in_block = true;
                out.push_str("  ");
                i += 2;
            } else if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
                out.push_str(&" ".repeat(b.len() - i));
                i = b.len();
            } else {
                out.push(char::from(b[i]));
                i += 1;
            }
        }
        out.push('\n');
    }
    out
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn ident_prefix(s: &str) -> String {
    s.chars().take_while(|c| is_ident(*c)).collect()
}

fn ident_suffix(s: &str) -> String {
    let tail: Vec<char> = s.chars().rev().take_while(|c| is_ident(*c)).collect();
    tail.into_iter().rev().collect()
}

fn last_ident_before_semicolon(t: &str) -> Option<String> {
    let name = ident_suffix(t.strip_suffix(';')?);
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// The workspace root, from this crate's manifest directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    //! The overlap check's own gate.
    //!
    //! It is here because [`check_overlap`] is a check that exists *because* a
    //! check silently stopped checking, and a duplicate-detector that quietly
    //! detected nothing would be the same failure one level up. These pin both
    //! directions: the shapes it must see, and the references it must not
    //! mistake for definitions.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::{check_overlap, defined_symbols};

    /// Every definition shape the two generated headers actually contain.
    const SHAPES: &str = "\
/*
 * A banner comment naming TFT_NOT_A_DEFINITION.
 */
#ifndef TF_TREE_H
#define TF_TREE_H

#include <stdint.h>

typedef struct tft_tree tft_tree;

#define TFT_MESSAGE_LEN 256

typedef int32_t tft_status;

typedef struct {
  uint32_t struct_size;
  tft_status code;
} tft_error;

/**
 * Doxy prose mentioning tft_plan_at and TFT_OK.
 */
tft_status tft_last_error(tft_error *out);

tft_status tft_plan_at_many(const tft_plan *plan,
                            const int64_t *stamps,
                            size_t count);

#endif  /* TF_TREE_H */
";

    #[test]
    fn every_definition_shape_is_seen_and_no_reference_is() {
        let got = defined_symbols(SHAPES);
        for want in [
            "TF_TREE_H",
            "TFT_MESSAGE_LEN",
            "tft_tree",
            "tft_status",
            "tft_error",
            "tft_last_error",
            "tft_plan_at_many",
        ] {
            assert!(got.contains(want), "{want} was not seen as a definition");
        }
        // Prose, parameter types and wrapped continuation lines are references.
        for never in [
            "TFT_NOT_A_DEFINITION",
            "TFT_OK",
            "tft_plan_at",
            "tft_plan",
            "stamps",
            "count",
            "out",
            "struct_size",
            "code",
        ] {
            assert!(
                !got.contains(never),
                "{never} is a reference, not a definition"
            );
        }
    }

    /// The regression this whole check exists for: a status code missing from
    /// [`super::STABLE`] is emitted into both headers, and the duplicate is an
    /// identical `#define` that no C compiler objects to.
    #[test]
    fn a_symbol_defined_by_both_headers_fails() {
        let stable = "#define TFT_ERR_ARENA_UNAVAILABLE -42\n";
        let unstable = "#include \"tf_tree.h\"\n#define TFT_ERR_ARENA_UNAVAILABLE -42\n";
        let err = check_overlap(stable, unstable).unwrap_err();
        assert!(
            err.contains("TFT_ERR_ARENA_UNAVAILABLE"),
            "the failure must name the symbol: {err}"
        );
    }

    /// And the unstable header naming stable symbols — which it does on nearly
    /// every line — is not an overlap.
    #[test]
    fn the_unstable_header_may_reference_the_stable_one() {
        let stable = "typedef int32_t tft_status;\ntft_status tft_tree_open(tft_tree **out);\n";
        let unstable = "#include \"tf_tree.h\"\n\
                        tft_status tft_tree_frame_count(const tft_tree *tree);\n";
        assert!(check_overlap(stable, unstable).is_ok());
    }

    /// The committed headers themselves, so the check is exercised against real
    /// `cbindgen` output and not only against the fixture above.
    #[test]
    fn the_committed_headers_do_not_overlap() {
        let inc = super::workspace_root().join("crates/tf_tree_c/include");
        let stable = std::fs::read_to_string(inc.join("tf_tree.h")).unwrap();
        let unstable = std::fs::read_to_string(inc.join("tf_tree_unstable.h")).unwrap();
        if let Err(e) = check_overlap(&stable, &unstable) {
            panic!("{e}");
        }
    }
}
