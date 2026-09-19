//! Did the kernel *grant* the huge pages the arena asked for?
//!
//! `MappedArena::attach` issues `MADV_HUGEPAGE` and `docs/PHASE5.md` §2.3 aligns a frozen arena to
//! 2 MiB for it, but nothing checks the advice was **taken**. It usually is not: a live arena is
//! shmem, governed by a different sysfs knob whose stock default is `never`:
//!
//! ```text
//! /sys/kernel/mm/transparent_hugepage/enabled        always [madvise] never
//! /sys/kernel/mm/transparent_hugepage/shmem_enabled  always within_size advise [never] deny force
//! ```
//!
//! So the request is issued and declined silently.
//!
//! # What this measures
//!
//! For the arena's address range in `/proc/self/smaps`: `ShmemPmdMapped` (bytes on huge pages; **the
//! grant**, zero means declined) and `Rss` (so zero on an untouched mapping is not read as a
//! refusal), alongside a lookup latency.
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
//! The arena must be **large**: huge pages come in 2 MiB units.
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

/// Dynamic edges. Wide rather than deep (`MAX_DEPTH` 32, `MAX_PATH_EDGES` 64): the point is arena *size*.
const EDGES: usize = 64;
/// Slots per ring: 64 edges x 16384 slots x 72 B is ~75 MiB, past the 2 MiB huge-page unit.
const SLOTS: u32 = 16_384;
/// Samples pushed per edge — the whole ring: a sparsely touched mapping cannot receive a huge page.
const FILL: usize = SLOTS as usize - 1;
const N: usize = 4_096;
const ROUNDS: usize = 21;

/// Huge-page and residency facts for one mapping, in kB as `smaps` reports them.
#[derive(Default, Debug, Clone, Copy)]
struct MapFacts {
    /// The mapping's start address, which decides whether a huge page is possible: shmem THP needs
    /// `vaddr` congruent to the file offset modulo 2 MiB, and `mmap(NULL, ..)` promises only page alignment.
    start: usize,
    size_kb: u64,
    rss_kb: u64,
    shmem_pmd_kb: u64,
    anon_huge_kb: u64,
    file_pmd_kb: u64,
    found: bool,
}

/// The `smaps` entry covering `addr`. Selected by *address*: a `memfd`, an anonymous mapping and a
/// `.tft` show three different names.
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
/// This separates the two ways a grant can be zero: `thp_*_fallback` rising means the kernel tried and
/// found no contiguous 2 MiB block (fragmentation, transient); every counter flat means it never
/// tried (policy, permanent). Matches the **whole** field name: `thp_fault_fallback` is a prefix of
/// `thp_fault_fallback_charge`.
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

/// Median ns per lookup, cycling across every edge so the whole arena is touched.
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
    // Any address inside the arena identifies its mapping; an edge record is one, reached via the read API.
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
