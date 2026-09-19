//! `docs/PHASE5.md` §2.1: a frozen `.tft` is read by the same `Plan::at` code as
//! a live arena. The same lookups run against both and must agree bit for bit.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use tf_tree::{
    Capacity, EdgeCfg, FrozenError, FrozenFileError, InterpPolicy, Iso3, Stamp, SystemDomain, Tree,
    TreeBuilder,
};

const MS: i64 = 1_000_000;

/// A private per-pid, per-tag directory holding one `.tft`, removed on drop.
///
/// `freeze_to` writes its temporary beside the target, so a directory of our own
/// lets the rename test assert that exactly the target file is present.
struct Scratch {
    dir: PathBuf,
    file: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("tf_tree_frozen-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{tag}.tft"));
        Scratch { dir, file }
    }
    fn path(&self) -> &Path {
        &self.file
    }
    /// What the target's parent directory holds.
    fn entries(&self) -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(&self.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A four-level tree with three dynamic edges and one static one.
///
/// Every pose is non-identity with rotation and translation on all axes, so
/// bit-equality cannot hold vacuously. Rings are lapped (2048 pushes into 512
/// slots) and the arena exceeds one `SNAPSHOT_CHUNK` (64 KiB), so both the wrap
/// and `write_frozen`'s copy loop are exercised.
fn fixture() -> Tree {
    let cfg = EdgeCfg::new(Capacity::slots(512)).interp(InterpPolicy::ScLerp);
    let tree = TreeBuilder::new()
        .dynamic_edge("map", "odom", cfg)
        .dynamic_edge("odom", "base_link", cfg)
        .static_edge(
            "base_link",
            "imu_mount",
            &tf_tree::exp_se3([0.1, -0.2, 0.3, 0.4, 0.5, -0.6]),
        )
        .dynamic_edge("imu_mount", "imu", cfg)
        .frame_headroom(4)
        .build()
        .unwrap();

    for (i, (parent, child)) in [("map", "odom"), ("odom", "base_link"), ("imu_mount", "imu")]
        .into_iter()
        .enumerate()
    {
        let p = tree.frame(parent).unwrap();
        let c = tree.frame(child).unwrap();
        let w = tree.claim(c, p).unwrap();
        let seed = 1.0 + i as f64;
        for k in 0..2048i64 {
            let t = k as f64 * 0.001 * seed;
            w.push(k * MS, &pose_at(seed, t)).unwrap();
        }
        // Releasing the claim would change the bytes under comparison.
        core::mem::forget(w);
    }
    tree
}

/// A pose with rotation and translation on every axis, distinct for every `t`.
fn pose_at(seed: f64, t: f64) -> Iso3 {
    tf_tree::exp_se3([
        0.30 * (t * std::f64::consts::SQRT_2).sin(),
        0.20 * (t * std::f64::consts::PI).cos(),
        0.17 * t + 0.05 * seed,
        1.30 * t + 0.11 * seed,
        -0.70 * (t * std::f64::consts::E).sin(),
        0.42 * (t + seed).cos(),
    ])
}

/// Frame pairs and stamps for every comparison; stamps fall between samples, so
/// answers are interpolated values the arena does not literally contain.
fn probes() -> Vec<(&'static str, &'static str, Stamp<SystemDomain>)> {
    let mut out = Vec::new();
    for (a, b) in [
        ("map", "imu"),
        ("imu", "map"),
        ("odom", "imu_mount"),
        ("base_link", "odom"),
        ("map", "base_link"),
    ] {
        for k in [1600i64, 1737, 1855, 1980, 2000] {
            out.push((a, b, Stamp::from_nanos(k * MS + 370_000)));
        }
    }
    out
}

/// **§2.1.** The same lookup against a live arena and its frozen `.tft` agrees bit
/// for bit.
///
/// Mutant: drop the `.add(done)` or the `+ done` in `write_frozen`'s copy loop
/// ⇒ fails (needs the multi-chunk fixture). A tail truncation survives: the
/// counter tail is zero here.
#[test]
fn a_frozen_lookup_is_bit_identical_to_the_live_one() {
    let live = fixture();
    let scratch = Scratch::new("bitident");
    live.freeze_to(
        scratch.path(),
        Some("unit-test"),
        [0; 32],
        1_700_000_000_000_000_000,
    )
    .unwrap();
    let frozen = Tree::open_frozen(scratch.path()).unwrap();

    let mut seen = Vec::new();
    for (a, b, stamp) in probes() {
        let l = live.lookup(a, b, stamp).unwrap();
        let f = frozen.lookup(a, b, stamp).unwrap();
        assert_eq!(
            l.to_bits(),
            f.to_bits(),
            "{a} -> {b} @ {stamp:?}: live and frozen disagree"
        );
        seen.push(l.to_bits());
    }

    // Guards against a degenerate fixture making the equality vacuous.
    assert!(
        seen.iter().all(|b| *b != Iso3::IDENTITY.to_bits()),
        "a probe returned the identity — the fixture is degenerate"
    );
    let unique: std::collections::HashSet<_> = seen.iter().collect();
    assert_eq!(unique.len(), seen.len(), "probes are not distinguishing");
}

/// **§5.6.** `freeze --from-live` carries the counter regions, with the live
/// arena's values.
///
/// Mutant: shorten `write_frozen`'s `arena_size` by the two counter regions ⇒
/// fails. Needs `unstable`, and this target needs `shm`: it runs in
/// `just shm-check`, not `just test`.
#[test]
#[cfg(feature = "unstable")]
fn freezing_carries_the_counter_regions() {
    let live = fixture();

    // Read from the topology block rather than guessing `EdgeId(1)`.
    let odom = live.frame("odom").unwrap();
    let edge = tf_tree::EdgeId(
        live.arena_view()
            .topology()
            .read_frame(odom)
            .expect("odom is in the topology")
            .2,
    );

    // `lookups_ok` reaches the arena only when the guard drops, so the scope
    // closes before the freeze; `err_extrap_after` needs a query past the newest.
    let map = live.frame("map").unwrap();
    let plan = live.plan(map, odom).unwrap();
    {
        let g = live.guard();
        for k in [1600i64, 1720, 1840] {
            plan.at(&g, Stamp::<SystemDomain>::from_nanos(k * MS))
                .unwrap();
        }
        assert!(
            plan.at(&g, Stamp::<SystemDomain>::from_nanos(9_000 * MS))
                .is_err(),
            "the extrapolation probe must fail, or it moves no error counter"
        );
    }

    let (ok, after) = {
        let view = live.arena_view();
        let c = view.edge_counters(edge).unwrap();
        (
            c.lookups_ok.load(std::sync::atomic::Ordering::Relaxed),
            c.err_extrap_after
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    };
    assert_eq!(
        ok, 3,
        "the denominator never moved; the fixture proves nothing"
    );
    assert_ne!(after, 0, "the error counter never moved");

    let scratch = Scratch::new("counters");
    live.freeze_to(scratch.path(), None, [0; 32], 0).unwrap();
    let frozen = Tree::open_frozen(scratch.path()).unwrap();

    let view = frozen.arena_view();
    let c = view.edge_counters(edge).unwrap();
    assert_eq!(c.lookups_ok.load(std::sync::atomic::Ordering::Relaxed), ok);
    assert_eq!(
        c.err_extrap_after
            .load(std::sync::atomic::Ordering::Relaxed),
        after
    );
}

/// A `.tft` is permanently read-only (§2.4): mutation is an error, not a
/// `SIGSEGV`.
///
/// Mutant: `ArenaBacking::Frozen`'s `is_writable` returns `true`, or bypass the
/// `is_writable` branch in `Tree::frame` ⇒ SIGSEGV.
#[test]
fn a_frozen_tree_refuses_every_mutation() {
    let live = fixture();
    let scratch = Scratch::new("readonly");
    live.freeze_to(scratch.path(), None, [0; 32], 0).unwrap();
    let frozen = Tree::open_frozen(scratch.path()).unwrap();

    // Faults under the first mutant.
    let g = frozen.guard();
    drop(g);
    assert!(frozen
        .lookup("map", "imu", Stamp::<SystemDomain>::from_nanos(1800 * MS))
        .is_ok());

    // Interning an unknown name would write through a `PROT_READ` mapping.
    assert!(
        frozen.frame("a_frame_that_was_never_declared").is_err(),
        "interning through a read-only mapping must not be attempted"
    );

    let p = frozen.frame("map").unwrap();
    let c = frozen.frame("odom").unwrap();
    assert!(
        frozen.claim(c, p).is_err(),
        "a frozen arena accepted a claim"
    );
    assert!(!frozen.is_shared());
}

/// A frozen tree refuses `await_frames` at once instead of sleeping through the
/// caller's budget (`docs/decisions/0019`).
///
/// The absent name is asked first against a 300 ms budget: only there does a
/// guard-less build poll and time out, so both assertions can see the mutant.
/// Mutants: `ArenaBacking::is_frozen` false for `Frozen`, or the guard removed
/// ⇒ `Timeout` instead of `FrozenTree`. `is_frozen` true for `Mapped` is caught
/// by `tests/rendezvous.rs`
/// `a_consumer_waits_for_a_frame_interned_after_the_arena_exists`.
#[test]
fn a_frozen_tree_refuses_to_wait_for_a_frame() {
    use std::time::{Duration, Instant};

    use tf_tree::AwaitError;

    let live = fixture();
    let scratch = Scratch::new("await-frozen");
    live.freeze_to(scratch.path(), None, [0; 32], 0).unwrap();
    let frozen = Tree::open_frozen(scratch.path()).unwrap();
    assert!(!frozen.is_writable(), "a .tft is permanently read-only");

    let budget = Duration::from_millis(300);
    let started = Instant::now();
    let absent = frozen.await_frames(["a_frame_that_was_never_declared"], budget);
    let elapsed = started.elapsed();
    assert_eq!(absent, Err(AwaitError::FrozenTree), "elapsed {elapsed:?}");
    assert!(
        elapsed < budget / 10,
        "a frozen arena has no writers, so waiting for an absent name is futile \
         by construction and must not be attempted: {elapsed:?}"
    );

    // Refused even for a present name: the refusal is about the handle.
    let present = frozen.await_frames(["map"], Duration::from_secs(5));
    assert_eq!(present, Err(AwaitError::FrozenTree));
}

/// A `.tft` with a different layout is refused, naming both hashes (§2.4).
///
/// Mutant: drop the `layout_hash` comparison in `FrozenArena::open`.
#[test]
fn a_stale_tft_is_refused_and_names_both_hashes() {
    use std::io::{Seek, SeekFrom, Write};

    let live = fixture();
    let scratch = Scratch::new("stale");
    let h = live.freeze_to(scratch.path(), None, [0; 32], 0).unwrap();

    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(scratch.path())
        .unwrap();
    // `layout_hash` is at offset 12 of the container header.
    f.seek(SeekFrom::Start(12)).unwrap();
    f.write_all(&(h.layout_hash ^ 0xDEAD).to_le_bytes())
        .unwrap();
    drop(f);

    assert_eq!(
        Tree::open_frozen(scratch.path()).err(),
        Some(FrozenFileError::Frozen(FrozenError::LayoutMismatch {
            found: h.layout_hash ^ 0xDEAD,
            expected: h.layout_hash,
        }))
    );
}

/// §2.3: the bytes go to a sibling temporary and are `rename`d over `path`.
///
/// The inode is the assertion: a value comparison passes even when `freeze_to`
/// writes in place. `rename` keeps an interrupted freeze from wearing the name
/// `open_frozen` will read, and keeps a currently mapped image intact.
///
/// Mutant: `File::create(path)` with no `rename` ⇒ the inode is unchanged.
#[test]
fn freezing_replaces_the_target_by_rename_not_in_place() {
    use std::os::unix::fs::MetadataExt;

    let live = fixture();
    let s = Scratch::new("rename");
    live.freeze_to(s.path(), Some("src"), [7; 32], 99).unwrap();
    let first_ino = std::fs::metadata(s.path()).unwrap().ino();

    // The mapping the rename exists to protect.
    let held = Tree::open_frozen(s.path()).unwrap();
    let (target, source, at) = probes()[0];
    let before = held
        .lookup(target, source, at)
        .expect("the held mapping must answer before the re-freeze");

    live.freeze_to(s.path(), Some("src"), [7; 32], 99).unwrap();

    assert_ne!(
        std::fs::metadata(s.path()).unwrap().ino(),
        first_ino,
        "freeze rewrote the target in place: a partial write would be visible at \
         `path`, and the mapping held open above would move under its reader"
    );
    let after = held.lookup(target, source, at).unwrap();
    assert_eq!(
        before.to_bits(),
        after.to_bits(),
        "the mapping held open across the freeze changed answers"
    );

    // Names what must be present rather than matching the private temporary
    // naming scheme, so a renamed scheme cannot make this pass vacuously.
    assert_eq!(
        s.entries(),
        vec![s.path().file_name().unwrap().to_owned()],
        "freeze left something beside the target in its own directory"
    );
}

/// Freezing the same arena twice gives the same bytes: the property is
/// determinism, not shortest-form CBOR (pinned by the RFC vectors in `cbor.rs`).
#[test]
fn freezing_twice_produces_the_same_bytes() {
    let live = fixture();
    let a = Scratch::new("repeat_a");
    let b = Scratch::new("repeat_b");
    live.freeze_to(a.path(), Some("src"), [7; 32], 99).unwrap();
    live.freeze_to(b.path(), Some("src"), [7; 32], 99).unwrap();
    assert_eq!(
        std::fs::read(a.path()).unwrap(),
        std::fs::read(b.path()).unwrap()
    );
}

/// `samples` is what the file holds (511 on a lapped 512 ring); `pushes_total`
/// is what was pushed (2048). The lapped fixture is what makes them differ.
///
/// Mutant: emit `e.head` for `samples` ⇒ both assertions fail.
#[test]
fn the_manifest_separates_what_the_file_holds_from_what_was_pushed() {
    let live = fixture();
    let scratch = Scratch::new("counts");
    let h = live.freeze_to(scratch.path(), None, [0; 32], 5).unwrap();
    let bytes = std::fs::read(scratch.path()).unwrap();
    let m = &bytes[h.manifest_off as usize..(h.manifest_off + h.manifest_len) as usize];

    fn count(hay: &[u8], needle: &[u8]) -> usize {
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    }

    // text(7) "samples" then uint16 511; text(12) "pushes_total" then uint16 2048.
    let retained = b"\x67samples\x19\x01\xff";
    let pushed = b"\x6cpushes_total\x19\x08\x00";
    assert_eq!(
        count(m, retained),
        3,
        "expected 511 retained samples on each of the 3 lapped edges"
    );
    assert_eq!(
        count(m, pushed),
        3,
        "expected 2048 total pushes on each of the 3 lapped edges"
    );
    assert_eq!(count(m, b"\x67samples\x19\x08\x00"), 0);
}

/// The manifest is CBOR naming the frames and edges §2.3 asks for, checked on the
/// encoded bytes (no CBOR dev-dependency).
///
/// Mutant: `w.array(frames - 1)` ⇒ the array header becomes `0x84`.
#[test]
fn the_manifest_is_cbor_and_names_the_frames() {
    let live = fixture();
    let scratch = Scratch::new("manifest");
    let h = live
        .freeze_to(scratch.path(), Some("bag.mcap"), [0; 32], 5)
        .unwrap();
    let bytes = std::fs::read(scratch.path()).unwrap();
    let m = &bytes[h.manifest_off as usize..(h.manifest_off + h.manifest_len) as usize];

    // A definite-length map of 7 pairs.
    assert_eq!(m[0], 0xA7, "manifest is not a 7-key CBOR map");
    // 5 frames were declared (map, odom, base_link, imu_mount, imu), so the
    // array header is 0x85 and each name is a text string.
    let frames_key = b"\x66frames"; // text(6) "frames"
    let at = m
        .windows(frames_key.len())
        .position(|w| w == frames_key)
        .expect("no `frames` key in the manifest");
    assert_eq!(m[at + frames_key.len()], 0x85, "expected 5 frame names");
    // text(3) "map"
    assert_eq!(
        &m[at + frames_key.len() + 1..at + frames_key.len() + 5],
        b"\x63map"
    );

    // The source path round-trips as a text string.
    assert!(m.windows(9).any(|w| w == b"\x68bag.mcap"));
}

/// The committed tag-1 fixture `testdata/frozen/sensor_domain.tft` still reads
/// and still carries tag 1 (`0038` step 4).
///
/// It fails naming the regenerator when `FORMAT_VERSION` or `layout_hash`
/// changes, in `just shm-check`. It is also the Rust-side red arm for a forgotten
/// region stride (`docs/decisions/0032-the-region-table-was-not-part-of-the-purchase.md`
/// question 1): `LayoutMismatch` for version skew, `HeaderInconsistent` for a
/// forgotten stride. Properties, not bytes: two freezes never match bytewise.
#[test]
fn the_committed_sensor_domain_fixture_reads_and_is_still_tag_one() {
    use tf_tree::{Domain, SensorDomain};

    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/frozen/sensor_domain.tft")
        .canonicalize()
        .expect(
            "testdata/frozen/sensor_domain.tft is missing; regenerate it with \
                 `cargo run -p tf_tree --features shm --example gen_domain_fixture`",
        );

    let tree = Tree::open_frozen(&path).expect(
        "the committed fixture no longer opens; if FORMAT_VERSION or the layout changed, \
         regenerate it with `cargo run -p tf_tree --features shm --example \
         gen_domain_fixture` and commit the result",
    );

    let map = tree.frame("map").unwrap();
    let base = tree.frame("base_link").unwrap();
    let lidar = tree.frame("lidar").unwrap();
    let plan = tree.plan(map, base).unwrap();

    assert_eq!(
        plan.domain(),
        SensorDomain::TAG,
        "the fixture stopped carrying a non-zero domain, which makes every Python \
         domain assertion vacuous rather than failing"
    );

    let g = tree.guard();
    let want = plan
        .at(&g, Stamp::<SensorDomain>::from_nanos(75_000_000))
        .expect("the fixture's own domain must answer");
    // The tagged spelling is what the bindings call.
    assert_eq!(plan.at_tagged(&g, 75_000_000, SensorDomain::TAG), Ok(want));
    // Tag 0 must not answer.
    assert_eq!(
        plan.at(&g, Stamp::<SystemDomain>::from_nanos(75_000_000)),
        Err(tf_tree::LookupError::TimeDomainMismatch {
            expected: 1,
            got: 0
        }),
        "a tag-0 query answered a tag-1 plan"
    );

    // A route through the static edge, folding more than one step.
    let through_static = tree.plan(map, lidar).unwrap();
    assert_eq!(through_static.domain(), SensorDomain::TAG);
    through_static
        .at(&g, Stamp::<SensorDomain>::from_nanos(75_000_000))
        .expect("the composed route must answer at the same stamp");
}
