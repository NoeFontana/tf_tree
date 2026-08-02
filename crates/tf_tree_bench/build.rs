//! Bake the ROS 2 library path into this crate's own binaries.
//!
//! `tf_tree_tf2_sys` links `libtf2.so` and emits `cargo:rustc-link-arg` with an
//! rpath — but that flag applies only to the *emitting* package's targets. It
//! never reaches this crate's benches, tests or bins, so before this file
//! existed they resolved `libtf2.so` purely through the `LD_LIBRARY_PATH` the
//! ROS container happens to set. Anywhere else — a host with ROS installed but
//! not sourced, a CI step that scrubs the environment, a binary copied out of
//! the container — they failed to start.
//!
//! The path comes from `tf_tree_tf2_sys`'s `links` metadata rather than being
//! rediscovered here, so there is exactly one ROS-discovery implementation in
//! the workspace and the two can never disagree.
//!
//! Without `--features tf2` there is no such dependency, hence no metadata, and
//! this script does nothing.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DEP_TF_TREE_TF2_SHIM_RPATH");
    emit_profile_dir();
    emit_source_id();

    let Ok(lib_dir) = std::env::var("DEP_TF_TREE_TF2_SHIM_RPATH") else {
        return; // built without `--features tf2`
    };

    // Every target kind that produces a runnable binary here: the criterion
    // benches, the integration tests, `tf2_scaling`, and the examples.
    //
    // `--disable-new-dtags` emits the old `DT_RPATH` rather than `DT_RUNPATH`,
    // and that is load-bearing: `DT_RUNPATH` is consulted only for the
    // executable's *own* direct dependencies, so `libtf2.so` would be found but
    // its `librcutils.so` — which carries no rpath of its own, being a ROS
    // library that expects a sourced environment — would not. `DT_RPATH` is
    // inherited down the whole dependency chain, which is what makes the binary
    // genuinely runnable without `LD_LIBRARY_PATH`.
    for kind in ["benches", "tests", "bins", "examples"] {
        println!("cargo:rustc-link-arg-{kind}=-Wl,--disable-new-dtags");
        println!("cargo:rustc-link-arg-{kind}=-Wl,-rpath,{lib_dir}");
    }
}

/// Bake the profile *directory* this crate was compiled into into its binaries.
///
/// `docs/PHASE5.md` §9.2's embedding row is two runs of the same program built
/// under two different `[profile.*]` sections, and its whole value is that the
/// two are not confused. Cargo tells a build script the opt-level and whether
/// debug assertions are on, but not the profile *name*, and neither `lto` nor
/// `codegen-units` — so a binary asked "which profile are you?" would otherwise
/// have to be told by whoever ran it. That is a provenance field the tool cannot
/// check and a reader has no reason to believe.
///
/// `OUT_DIR` carries it: cargo lays out `<target>/<profile-dir>/build/<pkg>/out`
/// (with an extra `<triple>` component when cross-compiling), so the component
/// immediately before `build` is the profile directory — `release` for
/// `--release`, `embedder` for `--profile embedder`, `debug` for a dev build.
/// It is a fact about where the object files went, not a label.
///
/// `crates/tf_tree_bench/src/embed.rs` maps that directory back to the profile's
/// declared `lto` and `codegen-units` by reading the workspace manifest, so the
/// chain from "this binary" to "`lto = false`, `codegen-units = 16`" has no
/// unchecked link.
fn emit_profile_dir() {
    let dir = std::env::var("OUT_DIR").ok().and_then(|out| {
        let path = std::path::PathBuf::from(out);
        let parts: Vec<String> = path
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect();
        parts
            .iter()
            .rposition(|c| c == "build")
            .and_then(|i| i.checked_sub(1))
            .map(|i| parts[i].clone())
    });
    // `unknown` rather than a panic: a panic here stops every target in this
    // crate from building over a provenance string one binary reads, and that
    // binary already refuses to report a run whose profile it cannot name.
    println!(
        "cargo:rustc-env=TF_TREE_BENCH_PROFILE_DIR={}",
        dir.unwrap_or_else(|| "unknown".to_owned())
    );
}

/// The set of source trees that determine what `embed_cost` measures.
///
/// Relative to the workspace root. The workspace manifest is in the list
/// because `[profile.embedder]` and `[profile.release]` live there and a change
/// to either changes the program without changing a line of Rust.
const MEASURED_SOURCES: &[&str] = &[
    "Cargo.toml",
    "crates/tf_tree_math/src",
    "crates/tf_tree_arena/src",
    "crates/tf_tree_core/src",
    "crates/tf_tree/src",
    "crates/tf_tree_bench/src/embed.rs",
    "crates/tf_tree_bench/src/fixture.rs",
];

/// Bake a digest of that source into the binary, so a report cannot be assembled
/// from two halves of different programs.
///
/// `docs/PHASE5.md` §9.2's embedding row is the *ratio* between two timed runs.
/// A ratio is a property of one program; pairing a fresh half with a stale one
/// produces a number that describes neither, and nothing about the two JSON
/// files would show it — they would differ only in a duration, which is what
/// they are supposed to differ in. `crates/tf_tree_bench/src/embed.rs` refuses a
/// pair whose halves disagree here.
///
/// A digest of the *source* rather than of the binary, because the two halves
/// are deliberately two different binaries: they are built under two
/// `[profile.*]` sections, so their bytes must differ and their sources must
/// not.
///
/// FNV-1a rather than a real hash: this is a collision check against accident,
/// not against an adversary, and `tf_tree_bench`'s build script has no
/// dependencies. Path names are hashed alongside contents so that moving a file
/// changes the digest.
fn emit_source_id() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut files = Vec::new();
    for rel in MEASURED_SOURCES {
        let path = root.join(rel);
        println!("cargo:rerun-if-changed={}", path.display());
        collect(&path, &mut files);
    }
    files.sort();
    for (name, bytes) in &files {
        for b in name.as_bytes().iter().chain(bytes.iter()) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    // `files.is_empty()` would hash to the FNV offset basis and silently claim
    // two unrelated builds agree. An empty set is a broken build script, not a
    // program with no source.
    let id = if files.is_empty() {
        "unknown".to_owned()
    } else {
        format!("{h:016x}")
    };
    println!("cargo:rustc-env=TF_TREE_BENCH_SOURCE_ID={id}");
}

/// Every `.rs`/`.toml` file under `path` (or `path` itself), as
/// `(file name, contents)`.
///
/// The **file name** and not the path: the digest must be the same for two
/// checkouts of the same commit in different directories, or the pairing check
/// would fire on a worktree rather than on a stale half. Two files sharing a
/// basename are distinguished by their contents, and the list is sorted on the
/// whole pair, so the digest stays order-independent either way.
fn collect(path: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for e in entries.flatten() {
            collect(&e.path(), out);
        }
        return;
    }
    let is_source = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e == "rs" || e == "toml");
    if !is_source {
        return;
    }
    if let Ok(bytes) = std::fs::read(path) {
        // The file name alone, not the absolute path: the digest must not
        // change because the checkout moved.
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push((name, bytes));
    }
}
