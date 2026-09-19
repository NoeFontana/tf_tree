//! Regenerate `testdata/frozen/sensor_domain.tft`, a frozen arena whose edges carry
//! a non-zero time domain (tag 1).
//!
//! ```sh
//! cargo run -p tf_tree --features shm --example gen_domain_fixture
//! ```
//!
//! [`0038`](../../../docs/decisions/0038-the-domain-a-binding-cannot-name.md) step 4
//! needs a Python test on a non-tag-0 arena, and Python cannot build one; by
//! `docs/PHASE5.md` §2.1 a `.tft` is read by the same `Plan::at`, so a committed
//! file needs no new API. `crates/tf_tree/tests/frozen.rs` asserts the file's
//! properties, not its bytes: the header carries a timestamp, pid, boot id and uuid.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

fn main() {
    #[cfg(all(feature = "shm", target_os = "linux"))]
    generate();
    #[cfg(not(all(feature = "shm", target_os = "linux")))]
    {
        eprintln!("gen_domain_fixture needs `--features shm` on Linux: writing a .tft maps memory");
        std::process::exit(1);
    }
}

#[cfg(all(feature = "shm", target_os = "linux"))]
fn generate() {
    use tf_tree::{Capacity, Domain, EdgeCfg, InterpPolicy, SensorDomain, TreeBuilder};

    /// Where the fixture lives, relative to the workspace root.
    const OUT: &str = "testdata/frozen/sensor_domain.tft";

    // Tag 1 on every dynamic edge, so a binding that gets the tag wrong is observable.
    let cfg = EdgeCfg::new(Capacity::slots(32))
        .interp(InterpPolicy::ScLerp)
        .domain(SensorDomain::TAG);

    // Small on purpose: two dynamic edges and one static one compose a route.
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", cfg)
        .dynamic_edge("odom", "base_link", cfg)
        .static_edge(
            "base_link",
            "lidar",
            &tf_tree::exp_se3([0.1, -0.2, 0.3, 0.4, 0.5, -0.6]),
        )
        .build()
        .unwrap();

    // Poses distinct in every component, so an identity-valued fixture cannot pass vacuously.
    for (i, (parent, child)) in [("map", "odom"), ("odom", "base_link")]
        .into_iter()
        .enumerate()
    {
        let p = tree.frame(parent).unwrap();
        let c = tree.frame(child).unwrap();
        let w = tree.claim(c, p).unwrap();
        let seed = 1.0 + i as f64;
        for k in 0..16i64 {
            let t = k as f64 * 0.01 * seed;
            w.push(
                k * 10_000_000, // 10 ms apart, so stamps 0..150 ms
                &tf_tree::exp_se3([
                    0.30 * (t * std::f64::consts::SQRT_2).sin(),
                    0.20 * (t * std::f64::consts::PI).cos(),
                    0.17 * t + 0.05 * seed,
                    1.30 * t + 0.11 * seed,
                    -0.70 * (t * std::f64::consts::E).sin(),
                    0.42 * (t + seed).cos(),
                ]),
            )
            .unwrap();
        }
        // Held for the freeze: releasing would clear the claim record.
        core::mem::forget(w);
    }

    let path = std::path::Path::new(OUT);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // `created_unix_ns = 0` so regeneration does not diff.
    let header = tree
        .freeze_to(path, Some("gen_domain_fixture"), [0; 32], 0)
        .unwrap();
    println!(
        "wrote {OUT}: {} bytes, format_version {}, layout_hash {:#010x}",
        std::fs::metadata(path).unwrap().len(),
        header.format_version,
        header.layout_hash,
    );
}
