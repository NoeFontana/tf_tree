//! Compile and link the `tf2::BufferCore` C++ shim.
//!
//! Discovers a ROS 2 install, compiles `src/shim.cpp` against it, and links
//! `libtf2`. If no ROS install is found the build fails with an actionable
//! message rather than a screen of missing-header errors — a missing ROS
//! toolchain is a configuration fact, not a compile error to decipher.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=src/shim.cpp");
    println!("cargo:rerun-if-env-changed=TF_TREE_ROS_PREFIX");
    println!("cargo:rerun-if-env-changed=ROS_DISTRO");

    let prefix = match find_ros_prefix() {
        Some(p) => p,
        None => {
            eprintln!(
                "\ntf_tree_tf2_sys: no ROS 2 install found.\n\
                 \n\
                 This crate bridges to ROS 2's `tf2::BufferCore` and only builds where\n\
                 ROS 2 is present. Either:\n\
                 \n\
                 * source a ROS 2 install (`. /opt/ros/<distro>/setup.bash`), or\n\
                 * point at one explicitly: TF_TREE_ROS_PREFIX=/opt/ros/<distro>, or\n\
                 * run the containerised harness: `just tf2-differential`\n\
                 \n\
                 Nothing else in the workspace depends on this crate; it is reached only\n\
                 through `tf_tree_bench --features tf2`.\n"
            );
            std::process::exit(1);
        }
    };

    let include_root = prefix.join("include");
    let lib_dir = prefix.join("lib");

    let mut build = cc::Build::new();
    build.cpp(true).std("c++17").file("src/shim.cpp");

    // ROS 2 headers live one package-directory deep (`include/<pkg>/<pkg>/x.hpp`),
    // so every immediate subdirectory of `include/` is an include root.
    let entries = match std::fs::read_dir(&include_root) {
        Ok(e) => e,
        Err(e) => {
            eprintln!(
                "tf_tree_tf2_sys: cannot read {}: {e}",
                include_root.display()
            );
            std::process::exit(1);
        }
    };
    let mut package_dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    package_dirs.sort(); // deterministic command line -> reproducible builds
    build.include(&include_root);
    for dir in &package_dirs {
        build.include(dir);
    }

    // ROS's own generated headers use a deprecated std::wstring_convert; that is
    // not our code and not something we can fix, so do not let it drown the log.
    build.flag_if_supported("-Wno-deprecated-declarations");

    build.compile("tf_tree_tf2_shim");

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=tf2");
    // The consuming test/bench binaries are run directly, not through
    // `ros2 run`, so bake the library path in rather than relying on
    // LD_LIBRARY_PATH being set at run time.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
}

/// Locate a ROS 2 install prefix, most explicit source first.
fn find_ros_prefix() -> Option<PathBuf> {
    // 1. Explicit override.
    if let Ok(p) = std::env::var("TF_TREE_ROS_PREFIX") {
        let p = PathBuf::from(p);
        if is_ros_prefix(&p) {
            return Some(p);
        }
        eprintln!(
            "tf_tree_tf2_sys: TF_TREE_ROS_PREFIX={} does not look like a ROS 2 \
             install (no include/tf2 or lib/libtf2.so)",
            p.display()
        );
        return None;
    }

    // 2. A sourced environment.
    if let Ok(distro) = std::env::var("ROS_DISTRO") {
        let p = PathBuf::from(format!("/opt/ros/{distro}"));
        if is_ros_prefix(&p) {
            return Some(p);
        }
    }

    // 3. Any install under /opt/ros; newest name last so the highest distro
    //    alphabetically wins deterministically.
    let mut found: Vec<PathBuf> = std::fs::read_dir("/opt/ros")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| is_ros_prefix(p))
        .collect();
    found.sort();
    found.pop()
}

/// Whether `p` holds the two things the shim actually needs.
fn is_ros_prefix(p: &Path) -> bool {
    p.join("include/tf2").is_dir() && p.join("lib/libtf2.so").exists()
}
