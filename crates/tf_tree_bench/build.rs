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
