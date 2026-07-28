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
    "TFT_ERR_INTERNAL",
    // Layouts — §3.5.
    "tft_layout",
    "TFT_LAYOUT_QVEC7_WXYZ",
    "TFT_LAYOUT_QVEC7_XYZW",
    "TFT_LAYOUT_MAT4_COL",
    "TFT_LAYOUT_MAT4_ROW",
    "TFT_LAYOUT_AFFINE12_ROW_F32",
    "tft_layout_size",
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
    "tft_bridge_note_queue_depth",
    "tft_bridge_get_stats",
    "tft_bridge_topic",
    "tft_bridge_authority",
    "tft_bridge_on_clock_reset",
    "tft_bridge_action",
    "tft_bridge_reason",
    "tft_bridge_sample",
    "tft_bridge_outcome",
    "tft_bridge_options",
    "tft_bridge_stats",
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
    "tft_bridge_sample",
    "tft_bridge_outcome",
    "tft_bridge_options",
    "tft_bridge_stats",
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

/// The workspace root, from this crate's manifest directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf()
}
