//! `docs/PHASE5.md` §2.1, end to end: **the file *is* the arena**.
//!
//! §2.1 is NORMATIVE that a frozen `.tft` is read by the identical `Plan::at`
//! code as a live arena, against a `PROT_READ` mapping, with no offline variant
//! of the lookup and no separate index. The only way to hold that claim to
//! account is to run the *same* lookups against both and demand **bit-for-bit**
//! agreement — not agreement to a tolerance, which would pass even if the frozen
//! path had quietly acquired its own interpolation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use tf_tree::{
    Capacity, EdgeCfg, FrozenError, FrozenFileError, InterpPolicy, Iso3, Stamp, SystemDomain, Tree,
    TreeBuilder,
};

const MS: i64 = 1_000_000;

/// A path in the temp dir that is removed when the test ends, pass or fail.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut p = std::env::temp_dir();
        p.push(format!("tf_tree_{tag}_{}.tft", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Scratch(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A four-level tree with three dynamic edges and one static one.
///
/// **Non-degeneracy is the whole point of this fixture.** Every pushed pose has
/// a rotation *and* a translation on all three axes, driven by irrational
/// multiples of the sample index, so no two samples agree in any component and
/// none of them is the identity. A fixture of identity poses — or of pure
/// translations, or of one sample per edge — would make "the frozen bits equal
/// the live bits" true for reasons that have nothing to do with the freeze
/// working, which is exactly the failure this file exists to avoid.
///
/// The rings are deliberately *lapped*: 2048 pushes into 512 slots, so `head`
/// exceeds capacity and the retained window has actually wrapped. A ring that
/// never wrapped would leave the physical layout equal to the logical one and
/// hide any index confusion introduced by relocation.
///
/// The size is also load-bearing. Three 512-slot rings put the arena at ~130 KB,
/// which is **more than one `SNAPSHOT_CHUNK`** (64 KiB), so `write_frozen`'s copy
/// loop actually iterates. At the 128-slot size this fixture started at, the
/// whole arena fitted in one chunk and every mutation of the loop's arithmetic
/// was a no-op that the bit comparison could not see.
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
        // The claim must stay held for the duration; releasing it would clear
        // the claim record and change the bytes under comparison for a reason
        // unrelated to freezing.
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

/// The frame pairs and stamps every comparison runs over.
///
/// The stamps land **between** sample instants (`+0.37 ms` of a 1 ms grid), so
/// every answer is an interpolated value the arena does not literally contain.
/// Comparing stored samples would only prove the bytes were copied; comparing
/// interpolated ones proves the *same interpolation ran over the same bits*.
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

/// **§2.1.** The same lookup against a live arena and against a `.tft` frozen
/// from it must agree bit for bit.
///
/// Mutant: drop the `.add(done)` from `write_frozen`'s source pointer, or the
/// `+ done` from its destination offset — the two ways a chunked copy loses its
/// cursor — ⇒ verified, this fails on the first probe. **Both needed the
/// enlarged fixture below**: at the 128-slot size this test started with, the
/// whole arena fitted in one `SNAPSHOT_CHUNK`, `done` was never non-zero, and
/// both mutants were no-ops the assertion could not see.
///
/// A *third* mutant is recorded here because it **survives**: truncating the
/// copy by the last 64 bytes changes nothing, because the arena's final region
/// is the participant-counter tail, which is zero in this fixture and which the
/// file is zero-filled with anyway. A tail truncation is only visible when the
/// tail is occupied, and nothing this test can build makes it so.
///
/// The two `assert!`s before the comparison are not decoration: without them a
/// fixture that produced identity everywhere, or one whose lookups all failed,
/// would satisfy the bit comparison while proving nothing.
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

    // The fixture must not be degenerate: no answer is the identity, and no two
    // probes agree. Either would make the equality above vacuous.
    assert!(
        seen.iter().all(|b| *b != Iso3::IDENTITY.to_bits()),
        "a probe returned the identity — the fixture is degenerate"
    );
    let unique: std::collections::HashSet<_> = seen.iter().collect();
    assert_eq!(unique.len(), seen.len(), "probes are not distinguishing");
}

/// **§5.6.** `freeze --from-live` carries the counter regions.
///
/// The section is NORMATIVE and the implementation satisfies it *structurally* —
/// the whole arena is copied, so `ArenaLayout::edge_counters()` and
/// `participant_counters()` land at their own offsets and are read back through
/// the identical accessor. This pins that, and pins that the values are the ones
/// the live arena had rather than zeros.
///
/// Mutant: shorten `write_frozen`'s `arena_size` by the two counter regions
/// (`64 * 128 + 8 * 128` bytes — i.e. stop at the end of the Phase 1 regions,
/// which is what a freeze written before v3 would do) ⇒ verified, this fails.
///
/// Both counters are made non-zero deliberately: `lookups_ok` is the §5.4
/// `Guard`-accumulated denominator and flushes on drop, so the guard is dropped
/// before the freeze; `err_extrap_after` is an error-path counter and needs a
/// query past the newest sample to move at all.
#[test]
fn freezing_carries_the_counter_regions() {
    let live = fixture();

    // The edge id comes from the topology block's `edge_of_child`, which is
    // where it lives — guessing `EdgeId(1)` would silently measure a different
    // edge if the builder ever reordered declarations.
    let odom = live.frame("odom").unwrap();
    let edge = tf_tree::EdgeId(
        live.arena_view()
            .topology()
            .read_frame(odom)
            .expect("odom is in the topology")
            .2,
    );

    // Both kinds of counter must move. `lookups_ok` is §5.4's `Guard`-batched
    // denominator, so it only reaches the arena when the guard drops — which is
    // why the scope closes before the freeze. `err_extrap_after` is an
    // error-path counter and needs a query past the newest sample.
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

/// A `.tft` is permanently read-only (§2.4), and that must be an error rather
/// than a `SIGSEGV`.
///
/// This is the one place where "read-only" has to be enforced in Rust rather
/// than by the MMU: the counter flush in `Guard::drop` is an unconditional
/// `fetch_add` on a writable view, so a frozen tree that reported itself
/// writable would kill the process on the *first lookup*, not on an attempted
/// publish. Mutant: make `ArenaBacking::Frozen`'s `is_writable` return `true` ⇒
/// verified, this test aborts with SIGSEGV on the `drop(g)` below. Second
/// mutant: bypass the `!self.arena.is_writable()` branch in `Tree::frame` ⇒
/// verified, SIGSEGV on the unknown-name probe.
#[test]
fn a_frozen_tree_refuses_every_mutation() {
    let live = fixture();
    let scratch = Scratch::new("readonly");
    live.freeze_to(scratch.path(), None, [0; 32], 0).unwrap();
    let frozen = Tree::open_frozen(scratch.path()).unwrap();

    // A lookup must still work — this is the line that faults under the mutant.
    let g = frozen.guard();
    drop(g);
    assert!(frozen
        .lookup("map", "imu", Stamp::<SystemDomain>::from_nanos(1800 * MS))
        .is_ok());

    // Resolving a name the file *does not* contain must be an error, not a
    // fault. `Tree::frame` interns on demand, and interning publishes into the
    // frame hash table with a `compare_exchange` — through a `PROT_READ`
    // mapping that is a `SIGSEGV`, and it is reachable from the most ordinary
    // possible typo. The read-only branch that catches it predates this
    // backend; what is new is that a frozen tree takes it.
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

/// A `.tft` written by a build with a different layout is refused, with both
/// hashes named (§2.4, NORMATIVE).
///
/// The file is otherwise perfect and only the `layout_hash` word is scrambled,
/// which is the exact shape of the real failure: the same tool, rebuilt after a
/// record grew. Mutant: drop the `layout_hash` comparison in
/// `FrozenArena::open` ⇒ the open succeeds and the assertion fails.
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

/// Freezing the same quiesced arena twice produces the same file, byte for byte
/// (modulo the timestamp the caller supplies).
///
/// This is what makes a `.tft` cacheable and diffable, and it is only true
/// because the manifest encoder uses CBOR's preferred (shortest-form)
/// serialization. Mutant: in `cbor::Writer::head`, always emit the 8-byte form
/// (`self.head`'s final branch) ⇒ still deterministic, so this test survives it
/// — the property it pins is determinism, *not* shortest-form, and the RFC
/// vectors in `cbor.rs` are what pin the latter.
/// §2.3: the bytes go to a **sibling temporary** and are `rename`d over `path`.
///
/// **The inode is the assertion**, and it has to be: freezing twice and comparing
/// the numbers read back passes just as well when `freeze_to` writes straight to
/// `path`, so until this test existed the property was asserted nowhere in the
/// repository — the whole suite passed with the `rename` deleted.
///
/// Why it matters is `write_frozen`'s `ftruncate`: an interrupted freeze leaves a
/// **full-length** file with a zeroed tail, not a short one. At a temporary name
/// that file is unlinked and forgotten. At `path` it is what next week's
/// `open_frozen` finds, and it fails `BadMagic` only because the header is
/// published last — a partial file that happened to keep a valid header would be
/// silently wrong instead. `rename` is what keeps such a file from ever wearing
/// the name somebody will open.
///
/// It is also what makes re-freezing over a *currently mapped* path safe, which
/// the second half checks: the open tree keeps the old inode and its answers.
///
/// Mutant: `File::create(path)` instead of `File::create(&tmp)` with the
/// `rename` removed ⇒ the inode is unchanged and the first assertion fails.
#[test]
fn freezing_replaces_the_target_by_rename_not_in_place() {
    use std::os::unix::fs::MetadataExt;

    let live = fixture();
    let s = Scratch::new("rename");
    live.freeze_to(s.path(), Some("src"), [7; 32], 99).unwrap();
    let first_ino = std::fs::metadata(s.path()).unwrap().ino();

    // Hold the first image open across the second freeze: this mapping is what
    // the rename exists to protect.
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

    // The temporary is a sibling and is gone. A temporary in `/tmp` would make
    // the `rename` non-atomic whenever `path` is on another filesystem.
    let dir = s.path().parent().unwrap();
    let stem = s.path().file_name().unwrap();
    let litter: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .filter(|n| {
            n != stem
                && n.to_string_lossy()
                    .contains(&format!("{}", std::process::id()))
        })
        .collect();
    assert!(litter.is_empty(), "freeze left litter: {litter:?}");
}

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

/// `samples` is what the **file** holds; `pushes_total` is what the source
/// pushed. On a lapped ring those differ by 4x, and conflating them is a rate
/// error, not a rounding one.
///
/// The fixture is load-bearing here in a way it is nowhere else: 2048 pushes
/// into 512 slots. On a ring that had *not* lapped both keys would carry the
/// same number and this test would pass no matter which one the encoder emitted
/// — which is precisely how the original defect survived review. 511, not 512,
/// because the slot at `head & mask` is the one being overwritten and is not
/// retained (`SampleRing::retained`).
///
/// Mutant: emit `e.head` for `samples` (the pre-amendment encoding) ⇒ the
/// `samples` value becomes `0x19 0x08 0x00` and both assertions below fail.
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
    // And the all-time count must never appear *as* `samples`.
    assert_eq!(count(m, b"\x67samples\x19\x08\x00"), 0);
}

/// The manifest is real CBOR and carries the frames and edges §2.3 asks for.
///
/// Decoded here by hand rather than with a CBOR crate, because adding one as a
/// dev-dependency to check a seven-key map would be a bigger commitment than the
/// thing being checked. The assertions are on the *encoded* bytes, which is the
/// strongest form: the frame name must appear as a length-prefixed text string
/// with the right prefix byte, so a length that disagreed with the payload would
/// fail. Mutant: emit `w.array(frames - 1)` and iterate `1..frames` — the
/// off-by-one that `edge_count`'s *opposite* convention invites, since that
/// field really does carry a sentinel — ⇒ verified, the array header byte
/// becomes `0x84` and the assertion fails.
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
