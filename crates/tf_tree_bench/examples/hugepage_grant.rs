//! Did the kernel *grant* the huge pages the arena asked for?
//!
//! `MappedArena::attach` issues `MADV_HUGEPAGE` (`mapped.rs`), and
//! `docs/PHASE5.md` §2.3 aligns a frozen arena to 2 MiB specifically so that
//! advice is meaningful — citing the arithmetic, a 115 MB index needs ~28 000
//! TLB entries on 4 KiB pages and 55 on 2 MiB ones. Nothing anywhere checks
//! whether the advice was **taken**.
//!
//! It usually is not. A live arena is a sealed `memfd` mapped `MAP_SHARED` —
//! shmem, not anonymous memory — and shmem huge pages are governed by a
//! *different* sysfs knob whose stock default is `never`:
//!
//! ```text
//! /sys/kernel/mm/transparent_hugepage/enabled        always [madvise] never
//! /sys/kernel/mm/transparent_hugepage/shmem_enabled  always within_size advise [never] deny force
//! ```
//!
//! So the request is issued, the kernel declines it silently, and every reader
//! of `PHASE5.md` §2.3 believes the arena is on 2 MiB pages.
//!
//! # What this measures
//!
//! For the arena's own address range, read from `/proc/self/smaps`:
//!
//! * `ShmemPmdMapped` — bytes actually backed by huge pages. **This is the
//!   grant.** Zero means the advice was declined.
//! * `Rss` — how much of the mapping is resident at all, so a zero grant on an
//!   untouched mapping is not mistaken for a refused one.
//!
//! and alongside it a lookup latency, so the two can be read together.
//!
//! # Running the counterfactual
//!
//! ```sh
//! cargo build --release -p tf_tree_bench --features shm --example hugepage_grant
//! taskset -c 2 ./target/release/examples/hugepage_grant            # as configured
//! echo advise | sudo tee /sys/kernel/mm/transparent_hugepage/shmem_enabled
//! taskset -c 2 ./target/release/examples/hugepage_grant            # with THP available
//! echo never  | sudo tee /sys/kernel/mm/transparent_hugepage/shmem_enabled
//! ```
//!
//! The arena must be **large** for this to mean anything: huge pages come in
//! 2 MiB units, so a 300 KiB arena can never receive one however the host is
//! configured. The default here is sized past that by a wide margin.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_precision_loss
)]

use std::hint::black_box;
use std::time::Instant;

use tf_tree::{Capacity, EdgeCfg, InterpPolicy, Stamp, SystemDomain, Tree, TreeBuilder};
use tf_tree_bench::fixture::dynamic_pose;
use tf_tree_core::EdgeId;

/// Dynamic edges. Wide rather than deep: `MAX_DEPTH` caps a chain at 16, and the
/// point is arena *size*, so the frames fan out from one root.
const EDGES: usize = 64;
/// Slots per ring. 64 edges x 16384 slots x 72 B/sample is ~75 MiB of arena —
/// well past the 2 MiB huge-page unit, and past the 32 MiB L3 too.
const SLOTS: u32 = 16_384;
/// Samples pushed per edge — the whole ring. Residency is the point: a huge
/// page is 2 MiB of *contiguous* memory, so a sparsely touched mapping cannot
/// receive one however the host is configured, and a partial fill would
/// under-report the grant rather than measure it.
const FILL: usize = SLOTS as usize - 1;
const N: usize = 4_096;
const ROUNDS: usize = 21;

/// Huge-page and residency facts for one mapping, in kB as `smaps` reports them.
#[derive(Default, Debug, Clone, Copy)]
struct MapFacts {
    /// The mapping's start address — the fact that decides whether a huge page
    /// is even *possible*. Shmem THP needs `vaddr` congruent to the file offset
    /// modulo 2 MiB, and `mmap(NULL, ..)` promises only page alignment.
    start: usize,
    size_kb: u64,
    rss_kb: u64,
    shmem_pmd_kb: u64,
    anon_huge_kb: u64,
    file_pmd_kb: u64,
    found: bool,
}

/// The `smaps` entry covering `addr`.
///
/// `/proc/self/smaps` is a sequence of `start-end perms ...` headers, each
/// followed by `Field:  N kB` lines. Selecting by *address* rather than by name
/// matters: a `memfd` shows up as `/memfd:tf_tree.… (deleted)`, an anonymous
/// mapping shows nothing, and a `.tft` shows its path — so matching on the name
/// would work for one backing and silently return zeros for the others.
fn map_facts(addr: usize) -> MapFacts {
    let mut out = MapFacts::default();
    let Ok(smaps) = std::fs::read_to_string("/proc/self/smaps") else {
        return out;
    };
    let mut in_range = false;
    for line in smaps.lines() {
        if let Some((range, _)) = line.split_once(' ') {
            if let Some((lo, hi)) = range.split_once('-') {
                if let (Ok(lo), Ok(hi)) =
                    (usize::from_str_radix(lo, 16), usize::from_str_radix(hi, 16))
                {
                    // A header line. Entering the target range or leaving it.
                    in_range = addr >= lo && addr < hi;
                    if in_range {
                        out.found = true;
                        out.start = lo;
                    }
                    continue;
                }
            }
        }
        if !in_range {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let Some(kb) = v.split_whitespace().next().and_then(|n| n.parse().ok()) else {
            continue;
        };
        match k {
            "Size" => out.size_kb = kb,
            "Rss" => out.rss_kb = kb,
            "ShmemPmdMapped" => out.shmem_pmd_kb = kb,
            "AnonHugePages" => out.anon_huge_kb = kb,
            "FilePmdMapped" => out.file_pmd_kb = kb,
            _ => {}
        }
    }
    out
}

/// The `/proc/vmstat` counters named by `keys`, in order; `None` for any absent.
///
/// **This is what separates the two ways a grant can be zero**, and without it
/// the harness can only say "no huge pages" and leave the reader to guess:
///
/// * `thp_*_fallback` rising ⇒ the kernel *tried* and could not find a
///   contiguous 2 MiB block. That is memory fragmentation, a host condition that
///   comes and goes.
/// * every counter flat ⇒ the kernel never tried at all. That is policy, or a
///   kernel that does not support it here — permanent, and not something the
///   arena can influence.
///
/// Matches the **whole** field name rather than a prefix, and reads the file
/// once. `/proc/vmstat` carries both `thp_fault_fallback` and
/// `thp_fault_fallback_charge`, and the former is a strict prefix of the latter,
/// so a `strip_prefix` match returns whichever the kernel happens to print
/// first — a silently wrong number rather than a missing one.
fn vmstat_all(keys: &[&str]) -> Vec<Option<u64>> {
    let text = std::fs::read_to_string("/proc/vmstat").unwrap_or_default();
    keys.iter()
        .map(|key| {
            text.lines().find_map(|l| {
                let (name, value) = l.split_once(' ')?;
                (name == *key).then(|| value.trim().parse().ok())?
            })
        })
        .collect()
}

fn read_sysfs(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<absent>".into())
}

fn build(shared: bool) -> (Tree, Vec<String>) {
    let names: Vec<String> = (0..=EDGES).map(|i| format!("f{i}")).collect();
    let mut b = TreeBuilder::new().default_interp(InterpPolicy::LerpSlerp);
    for i in 0..EDGES {
        b = b.dynamic_edge(
            &names[0],
            &names[i + 1],
            EdgeCfg::new(Capacity::slots(SLOTS)),
        );
    }
    let tree = if shared {
        b.build_shared("tf_tree.hugepage_grant").unwrap()
    } else {
        b.build().unwrap()
    };

    let mut writers = Vec::new();
    for i in 0..EDGES {
        let p = tree.frame(&names[0]).unwrap();
        let c = tree.frame(&names[i + 1]).unwrap();
        writers.push(tree.claim(c, p).unwrap());
    }
    for k in 0..FILL {
        let stamp = k as i64 * 1_000_000;
        for (i, w) in writers.iter().enumerate() {
            w.push(stamp, &dynamic_pose(i as f64, stamp)).unwrap();
        }
    }
    drop(writers);
    (tree, names)
}

/// Median ns per lookup, cycling across every edge so the whole arena is
/// touched rather than one hot ring.
fn latency(tree: &Tree, names: &[String]) -> f64 {
    let root = tree.frame(&names[0]).unwrap();
    let plans: Vec<_> = (0..EDGES)
        .map(|i| {
            let leaf = tree.frame(&names[i + 1]).unwrap();
            tree.plan(leaf, root).unwrap()
        })
        .collect();
    let guard = tree.guard();
    let stamps: Vec<i64> = (0..N as i64)
        .map(|k| (k % (FILL as i64 - 2) + 1) * 1_000_000 + 500_000)
        .collect();

    let run = || {
        let mut acc = 0.0;
        for (j, &ns) in stamps.iter().enumerate() {
            let stamp: Stamp<SystemDomain> = Stamp::from_nanos(ns);
            if let Ok(p) = plans[j % EDGES].at(&guard, stamp) {
                acc += p.t.x;
            }
        }
        acc
    };
    for _ in 0..3 {
        black_box(run());
    }
    let mut v = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t0 = Instant::now();
        let r = run();
        v.push(t0.elapsed().as_nanos() as f64 / stamps.len() as f64);
        black_box(r);
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn report(label: &str, tree: &Tree, names: &[String]) {
    // Any address inside the arena identifies its mapping, and an edge record
    // is one — reached through the ordinary read API, so this needs no unsafe
    // and no new accessor on the facade.
    let guard = tree.guard();
    let addr = core::ptr::from_ref(guard.view().edge(EdgeId(1)).expect("edge 1")) as usize;
    let f = map_facts(addr);
    let ns = latency(tree, names);

    println!("\n## {label}");
    if !f.found {
        println!("  (no smaps entry covers {addr:#x} — /proc unavailable?)");
    }
    const HUGE: usize = 2 * 1024 * 1024;
    println!("  mapping start     {:>#12x}", f.start);
    println!(
        "  2 MiB alignment   {:>10} {}",
        f.start % HUGE,
        if f.start.is_multiple_of(HUGE) {
            "<- aligned; a huge page is possible"
        } else {
            "<- NOT aligned; no huge page can ever cover offset 0"
        }
    );
    println!("  mapping size      {:>10} KiB", f.size_kb);
    println!("  resident (Rss)    {:>10} KiB", f.rss_kb);
    println!(
        "  ShmemPmdMapped    {:>10} KiB   <- the grant, for a MAP_SHARED memfd",
        f.shmem_pmd_kb
    );
    println!("  AnonHugePages     {:>10} KiB", f.anon_huge_kb);
    println!("  FilePmdMapped     {:>10} KiB", f.file_pmd_kb);
    let huge = f.shmem_pmd_kb + f.anon_huge_kb + f.file_pmd_kb;
    println!(
        "  huge-backed       {:>10.1} % of resident",
        if f.rss_kb == 0 {
            0.0
        } else {
            100.0 * huge as f64 / f.rss_kb as f64
        }
    );
    println!("  lookup p50        {ns:>10.1} ns");
}

fn main() {
    println!("tf_tree huge-page grant");
    println!("=======================");
    println!(
        "  transparent_hugepage/enabled        {}",
        read_sysfs("/sys/kernel/mm/transparent_hugepage/enabled")
    );
    println!(
        "  transparent_hugepage/shmem_enabled  {}   <- governs the live arena",
        read_sysfs("/sys/kernel/mm/transparent_hugepage/shmem_enabled")
    );
    println!(
        "\n  {EDGES} edges x {SLOTS} slots, {FILL} samples each; the arena is ~{} MiB",
        (EDGES as u64 * u64::from(SLOTS) * 72) / (1024 * 1024)
    );

    let before = vmstat_all(THP_COUNTERS);

    let (heap, names) = build(false);
    report(
        "heap arena (anonymous — governed by 'enabled')",
        &heap,
        &names,
    );
    drop(heap);

    let (shared, names) = build(true);
    report(
        "shared arena (MAP_SHARED memfd — governed by 'shmem_enabled')",
        &shared,
        &names,
    );

    // If the grant above was zero, this says which kind of zero it was.
    println!("\n## /proc/vmstat, across this run");
    println!(
        "{:>26} {:>10} {:>10} {:>10}",
        "counter", "before", "after", "delta"
    );
    let after = vmstat_all(THP_COUNTERS);
    for ((k, b), a) in THP_COUNTERS.iter().zip(before).zip(after) {
        match (b, a) {
            (Some(b), Some(a)) => println!("{k:>26} {b:>10} {a:>10} {:>10}", a - b),
            _ => println!("{k:>26} {:>10} {:>10} {:>10}", "-", "-", "-"),
        }
    }
    println!(
        "  *_fallback rising -> the kernel tried and fragmentation beat it (a transient host state)"
    );
    println!(
        "  everything flat   -> the kernel never attempted one; policy, or no THP support here"
    );
}

/// The four counters that describe a huge-page allocation attempt. `file` covers
/// shmem, which is what a live arena's `memfd` is.
const THP_COUNTERS: &[&str] = &[
    "thp_file_alloc",
    "thp_file_fallback",
    "thp_fault_alloc",
    "thp_fault_fallback",
];
