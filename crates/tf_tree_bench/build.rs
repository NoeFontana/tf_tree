//! Bake the ROS 2 library path into this crate's own binaries: `tf_tree_tf2_sys`'s
//! rpath covers only its own targets. The path comes from that crate's `links`
//! metadata. Without `--features tf2` this script does nothing.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DEP_TF_TREE_TF2_SHIM_RPATH");
    emit_profile_dir();
    emit_source_id();

    let Ok(lib_dir) = std::env::var("DEP_TF_TREE_TF2_SHIM_RPATH") else {
        return; // built without `--features tf2`
    };

    // `--disable-new-dtags` emits `DT_RPATH`, not `DT_RUNPATH`, and that is load-bearing: `RUNPATH`
    // covers only direct dependencies, so `libtf2.so`'s `librcutils.so` would not be found.
    for kind in ["benches", "tests", "bins", "examples"] {
        println!("cargo:rustc-link-arg-{kind}=-Wl,--disable-new-dtags");
        println!("cargo:rustc-link-arg-{kind}=-Wl,-rpath,{lib_dir}");
    }
}

/// Bake the profile *directory* this crate was compiled into into its binaries.
///
/// `docs/PHASE5.md` §9.2's embedding row is two runs under two `[profile.*]`
/// sections; cargo does not expose the profile name, but `OUT_DIR`'s component
/// before `build` is the profile directory, which `src/embed.rs` maps back to
/// the declared `lto` and `codegen-units`.
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
    // `unknown` rather than a panic, which would stop every target building.
    println!(
        "cargo:rustc-env=TF_TREE_BENCH_PROFILE_DIR={}",
        dir.unwrap_or_else(|| "unknown".to_owned())
    );
}

/// Source trees that determine what `embed_cost` measures, relative to the
/// workspace root (the manifest holds `[profile.*]`).
const MEASURED_SOURCES: &[&str] = &[
    "Cargo.toml",
    "crates/tf_tree_math/src",
    "crates/tf_tree_arena/src",
    "crates/tf_tree_core/src",
    "crates/tf_tree/src",
    "crates/tf_tree_bench/src/embed.rs",
    "crates/tf_tree_bench/src/fixture.rs",
];

/// Bake a digest of that source into the binary, so a report cannot pair halves
/// of different programs (`src/embed.rs` refuses a mismatch). The source is
/// hashed, not the binary; FNV-1a, guarding against accident. Each file's base
/// name (not path) is hashed with its contents (see [`collect`]).
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
    // An empty set would hash to the FNV offset basis and claim two unrelated builds agree.
    let id = if files.is_empty() {
        "unknown".to_owned()
    } else {
        format!("{h:016x}")
    };
    println!("cargo:rustc-env=TF_TREE_BENCH_SOURCE_ID={id}");
}

/// Every `.rs`/`.toml` file under `path` (or `path` itself), as `(file name,
/// contents)`, sorted so the digest is order-independent.
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
        // The file name alone: the digest must not change because the checkout moved.
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push((name, bytes));
    }
}
