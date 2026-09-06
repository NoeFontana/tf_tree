//! What `mlock` actually does to an arena-shaped mapping — `docs/API.md` §8.3's
//! executor, and [`0049`](../../../docs/decisions/0049-the-flag-that-prefaults-the-arena.md)'s.
//!
//! §8.3 asserts syscall behaviour and, until `0049`, reproduced no probe —
//! against `PHASE2.md`'s own preamble rule that *"where a syscall behaviour is
//! asserted, it has been verified on Linux 6.18; the probe is reproduced in
//! Appendix B"*. One of its assertions was wrong and one was a policy
//! conclusion in the grammar of a mechanism fact, and neither could be doubted
//! by anyone without a C compiler and an afternoon. This is the afternoon,
//! committed.
//!
//! # It is a probe and never a gate
//!
//! It prints and exits 0. Every arm's answer is a property of the running
//! kernel, the cgroup the process is in, and whether the host has swap; a
//! recipe asserting any of them would be a gate about the machine rather than
//! about this code. `docs/benchmarks/EVIDENCE.md` registers it as a probe, and
//! the numbers `0049` quotes are dated readings from it rather than live
//! claims.
//!
//! # The mapping under test
//!
//! A `memfd`, `ftruncate`d and mapped `MAP_SHARED` — the shape
//! `tf_tree_arena::mapped` gives a live arena — populated with
//! `MADV_POPULATE_WRITE`, which is what `PHASE2.md` §7.1 and
//! [`0024`](../../../docs/decisions/0024-population-is-per-edge-at-take-up.md)
//! already do per edge at take-up. It is deliberately **not** a `tf_tree` arena:
//! the questions here are about the kernel's treatment of shmem pages, and
//! building a tree would put a claim protocol between the syscall and the
//! answer.
//!
//! # Running it
//!
//! ```sh
//! cargo run --release -p tf_tree_bench --example mlock_probe
//! cargo run --release -p tf_tree_bench --example mlock_probe -- retention
//! ```
//!
//! With no argument it runs every arm, each in a **child process**, because
//! `mlockall(2)` is process-wide and an arm that calls it would contaminate
//! every arm after it.
//!
//! # What it cannot answer, and the arm that says so
//!
//! `MADV_PAGEOUT` is a **directed** reclaim: it isolates the named folios by
//! address and forces `reclaim_pages()`. The LRU scanner selects by pressure.
//! So the `retention` arm establishes that the teardown *mechanism* is not
//! blocked by swaplessness and establishes nothing about whether a kernel under
//! organic pressure would choose these folios. The `pressure` arm is the
//! organic half and it needs a memory cgroup to be meaningful — run it under
//! one, and read its file-backed positive control before believing its shmem
//! result:
//!
//! ```sh
//! systemd-run --user --scope -p MemoryMax=96M -q \
//!     ./target/release/examples/mlock_probe pressure
//! systemd-run --user --scope -p MemoryMax=96M -q \
//!     ./target/release/examples/mlock_probe pressure-file
//! ```
//!
//! Global `kswapd` pressure is reachable by neither and stays untested.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
// **`docs/decisions/0007` rule 1, kind 2 — the OS**, and the posture is declared
// here rather than inherited: `crates/tf_tree_bench/src/lib.rs` is
// `#![forbid(unsafe_code)]` and an example is a **separate crate root**, so that
// attribute governs none of this file.
// [`0049`](../../../docs/decisions/0049-the-flag-that-prefaults-the-arena.md)
// is why the file exists and
// [`0048`](../../../docs/decisions/0048-a-kind-is-not-a-crate-name.md) is why it
// is eligible: a kind is a property, and `mmap`/`mlock2`/`mlockall`/`madvise`
// are the OS boundary wherever they are called from.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

// SAFETY (module invariant): every `unsafe` block below is a libc call on a
// mapping this file created and still owns. `LEN` is the length passed to
// `ftruncate` and to `mmap`, and it is the length passed to every subsequent
// `mlock2`/`munlock`/`madvise`, so no call names a byte outside the mapping.
// The mappings are never unmapped: each arm is a short-lived process and exit
// is what releases them, which is also why no arm frees before it prints.

use std::io::Write;

/// 64 MiB — sixteen 2 MiB huge-page units, and enough that `Rss` in kB is
/// legible at a glance. Nothing here depends on the exact size.
const LEN: usize = 64 << 20;
/// The page size every arm strides by. Read rather than assumed would be more
/// portable; this file is `cfg`-ed to Linux and 4 KiB is what it runs on, and a
/// wrong value here changes only how many of the mapping's pages are touched.
const PAGE: usize = 4096;

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("mlock_probe: Linux only — mlock2, MADV_PAGEOUT and /proc/self/smaps");
}

#[cfg(target_os = "linux")]
fn main() {
    let arm = std::env::args().nth(1);
    match arm.as_deref() {
        Some("onfault") => linux::onfault(),
        Some("retention") => linux::retention(),
        Some("mlockall") => linux::mlockall_prefaults(),
        Some("mlockall-onfault") => linux::mlockall_onfault(),
        Some("refault-cost") => linux::refault_cost(),
        Some("memlock-limit") => linux::memlock_limit(&std::env::args().collect::<Vec<_>>()),
        Some("memlock-limit-child") => {
            linux::memlock_limit_child(&std::env::args().collect::<Vec<_>>());
        }
        Some("pressure") => linux::pressure(false),
        Some("pressure-file") => linux::pressure(true),
        Some(other) => {
            eprintln!("mlock_probe: unknown arm {other}");
            std::process::exit(2);
        }
        None => linux::run_all(),
    }
    let _ = std::io::stdout().flush();
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{LEN, PAGE};
    use std::ffi::CString;
    use std::io::{BufRead, BufReader};

    /// Every arm the no-argument run drives, each in its own process.
    ///
    /// `mlockall(2)` is process-wide, so the two arms that call it must not
    /// share an address space with anything measured afterwards.
    const ARMS: [&str; 6] = [
        "onfault",
        "retention",
        "mlockall",
        "mlockall-onfault",
        "refault-cost",
        "memlock-limit",
    ];

    pub(crate) fn run_all() {
        let me = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("mlock_probe: cannot find my own path: {e}");
                std::process::exit(2);
            }
        };
        for arm in ARMS {
            match std::process::Command::new(&me).arg(arm).status() {
                Ok(s) if s.success() => {}
                Ok(s) => println!("  ({arm} exited {s})"),
                Err(e) => println!("  ({arm} did not start: {e})"),
            }
            println!();
        }
        println!(
            "The organic-pressure arms are not run here: they need a memory cgroup to \
             mean anything, and one of them is expected to be OOM-killed. See this \
             file's header for the two `systemd-run` lines."
        );
    }

    /// `Rss:` in kB for the mapping starting at `p`, read from `/proc/self/smaps`.
    ///
    /// Keyed on the start address so a second mapping in the same process is not
    /// mistaken for this one — which matters in the two `mlockall` arms, where
    /// the point is precisely that a *second* mapping behaves differently.
    fn rss_kb(p: *mut libc::c_void) -> i64 {
        let want = p as usize;
        let Ok(f) = std::fs::File::open("/proc/self/smaps") else {
            return -1;
        };
        let mut here = false;
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if let Some((start, _)) = line.split_once('-') {
                if let Ok(a) = usize::from_str_radix(start, 16) {
                    here = a == want;
                }
            }
            if here {
                if let Some(v) = line.strip_prefix("Rss:") {
                    return v
                        .trim()
                        .trim_end_matches(" kB")
                        .parse::<i64>()
                        .unwrap_or(-1);
                }
            }
        }
        -1
    }

    /// A `memfd`, `ftruncate`d to `LEN` and mapped `MAP_SHARED` — a live arena's
    /// shape. `populate` runs §7.1's own `MADV_POPULATE_WRITE` over the whole of
    /// it.
    fn arena(populate: bool) -> *mut libc::c_void {
        let name = CString::new("mlock_probe").unwrap_or_default();
        // SAFETY: `name` is a live NUL-terminated string for the duration of the
        // call, which is all `memfd_create` reads.
        let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(fd >= 0, "memfd_create: {}", last_error());
        // SAFETY: `fd` is the descriptor just returned and is still open.
        let rc = unsafe { libc::ftruncate(fd, LEN as libc::off_t) };
        assert!(rc == 0, "ftruncate: {}", last_error());
        // SAFETY: a null hint asks the kernel to choose the address; `fd` is a
        // live memfd of exactly `LEN` bytes, so the whole mapping is backed.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        assert!(p != libc::MAP_FAILED, "mmap: {}", last_error());
        // SAFETY: `fd` is ours and the mapping keeps the file alive.
        unsafe { libc::close(fd) };
        if populate {
            // SAFETY: `p`/`LEN` is exactly the mapping created above.
            let rc = unsafe { libc::madvise(p, LEN, libc::MADV_POPULATE_WRITE) };
            assert!(rc == 0, "MADV_POPULATE_WRITE: {}", last_error());
        }
        p
    }

    fn last_error() -> String {
        std::io::Error::last_os_error().to_string()
    }

    fn advise(p: *mut libc::c_void, advice: libc::c_int) -> (i32, String) {
        // SAFETY: `p`/`LEN` is a mapping this process created and has not
        // unmapped.
        let rc = unsafe { libc::madvise(p, LEN, advice) };
        (rc, if rc == 0 { "ok".into() } else { last_error() })
    }

    fn lock_onfault(p: *mut libc::c_void) -> (i32, String) {
        // SAFETY: same mapping, same length. `MLOCK_ONFAULT` = 1; libc does not
        // export the constant for every target, and it is ABI, not policy.
        let rc = unsafe { libc::mlock2(p, LEN, 1) };
        (rc, if rc == 0 { "ok".into() } else { last_error() })
    }

    /// §8.3's first clause: *"`MLOCK_ONFAULT` would not prefault"*.
    pub(crate) fn onfault() {
        let p = arena(false);
        let (rc, err) = lock_onfault(p);
        println!("onfault: mlock2(MLOCK_ONFAULT) on an untouched mapping");
        println!("  rc={rc} ({err})  Rss={} kB", rss_kb(p));
        println!("  expected 0 kB — the flag locks what is faulted in, and nothing is");
    }

    /// §8.3's second clause: *"so it adds nothing over §7.1"*.
    ///
    /// One mapping, `VM_LOCKED` the only variable between the two `MADV_PAGEOUT`
    /// calls.
    pub(crate) fn retention() {
        let p = arena(true);
        println!("retention: population and locking are different things");
        println!("  after MADV_POPULATE_WRITE       Rss={} kB", rss_kb(p));
        let (rc, err) = lock_onfault(p);
        println!("  mlock2(MLOCK_ONFAULT)           rc={rc} ({err})");
        let (rc, err) = advise(p, libc::MADV_PAGEOUT);
        println!(
            "  MADV_PAGEOUT while locked       rc={rc} ({err})  Rss={} kB",
            rss_kb(p)
        );
        // SAFETY: unlocking exactly the range locked above.
        let rc = unsafe { libc::munlock(p, LEN) };
        println!("  munlock                         rc={rc}");
        let (rc, err) = advise(p, libc::MADV_PAGEOUT);
        println!(
            "  MADV_PAGEOUT after munlock      rc={rc} ({err})  Rss={} kB",
            rss_kb(p)
        );
        println!(
            "  the same syscall on the same range, so what changed is VM_LOCKED — \
             but MADV_PAGEOUT is DIRECTED reclaim and says nothing about what an \
             LRU scanner would select"
        );
    }

    /// §8.3's *recommendation*, which is the clause an operator acts on.
    pub(crate) fn mlockall_prefaults() {
        let p = arena(false);
        println!("mlockall: what MCL_CURRENT|MCL_FUTURE costs an over-provisioned arena");
        println!("  untouched                       Rss={} kB", rss_kb(p));
        // SAFETY: no arguments beyond the flag word; process-wide, which is why
        // this arm runs in its own process.
        let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
        let err = if rc == 0 { "ok".into() } else { last_error() };
        println!(
            "  mlockall(CUR|FUT)  rc={rc} ({err})  same region Rss={} kB",
            rss_kb(p)
        );
        let q = arena(false);
        println!(
            "  a NEW untouched mapping made after the call  Rss={} kB",
            rss_kb(q)
        );
        println!("  MCL_FUTURE is not only about future locks: it prefaults future mappings");
    }

    /// The flag §8.3 dismissed, at address-space scope.
    pub(crate) fn mlockall_onfault() {
        let p = arena(false);
        // SAFETY: as above; `MCL_ONFAULT` is the address-space spelling of
        // `MLOCK_ONFAULT`.
        let rc =
            unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE | libc::MCL_ONFAULT) };
        let err = if rc == 0 { "ok".into() } else { last_error() };
        println!("mlockall-onfault: the same call with MCL_ONFAULT added");
        println!(
            "  mlockall(CUR|FUT|ONFAULT)  rc={rc} ({err})  untouched Rss={} kB",
            rss_kb(p)
        );
        let q = arena(false);
        println!("  a NEW untouched mapping         Rss={} kB", rss_kb(q));
        // SAFETY: `q`/`LEN` is the mapping just created; writing through it is
        // what faults its pages in.
        unsafe { std::ptr::write_bytes(q.cast::<u8>(), 1, LEN) };
        println!("  after touching it               Rss={} kB", rss_kb(q));
        let (rc, err) = advise(q, libc::MADV_PAGEOUT);
        println!(
            "  MADV_PAGEOUT on it              rc={rc} ({err})  Rss={} kB",
            rss_kb(q)
        );
    }

    /// What a refault costs, which is the quantity §8.3 exists to protect.
    ///
    /// **The accumulator is read after the loop.** A warm-read loop whose result
    /// is discarded compiles away at `-O2`, and a comparator taken that way
    /// reports single-digit microseconds for work that cannot be done in them.
    pub(crate) fn refault_cost() {
        let p = arena(true);
        let base = p.cast::<u8>();
        let mut acc: u64 = 0;
        let t0 = std::time::Instant::now();
        for i in (0..LEN).step_by(PAGE) {
            // SAFETY: `i < LEN` and the whole mapping is readable and populated.
            acc += u64::from(unsafe { std::ptr::read_volatile(base.add(i)) });
        }
        let warm = t0.elapsed();
        let (rc, err) = advise(p, libc::MADV_PAGEOUT);
        let after = rss_kb(p);
        let before = fault_counts();
        let t0 = std::time::Instant::now();
        for i in (0..LEN).step_by(PAGE) {
            // SAFETY: as above — MADV_PAGEOUT drops the PTEs, it does not
            // shrink the mapping.
            acc += u64::from(unsafe { std::ptr::read_volatile(base.add(i)) });
        }
        let cold = t0.elapsed();
        let faults = fault_counts();
        println!("refault-cost: one page per 4 KiB, accumulator consumed");
        println!(
            "  warm read                       {:.1} us",
            warm.as_secs_f64() * 1e6
        );
        println!("  MADV_PAGEOUT                    rc={rc} ({err})  Rss={after} kB");
        println!(
            "  cold re-read                    {:.1} us  minflt={} majflt={}",
            cold.as_secs_f64() * 1e6,
            faults.0 - before.0,
            faults.1 - before.1
        );
        println!("  accumulator (kept so the loops are not dead code): {acc}");
        println!(
            "  zero major faults with no swap on the host is the folio surviving in \
             page cache while the mapping did not"
        );
    }

    /// `(minor, major)` faults for this process, from `/proc/self/stat`.
    ///
    /// Fields 10 and 12, one-based, after the `comm` field — which is
    /// parenthesised and may contain spaces, so the split starts after the last
    /// `)`.
    fn fault_counts() -> (u64, u64) {
        let Ok(s) = std::fs::read_to_string("/proc/self/stat") else {
            return (0, 0);
        };
        let Some(rest) = s.rsplit_once(')').map(|(_, r)| r) else {
            return (0, 0);
        };
        let f: Vec<&str> = rest.split_whitespace().collect();
        // `rest` starts at field 3 (`state`), so field 10 is index 7.
        let get = |i: usize| f.get(i).and_then(|v| v.parse().ok()).unwrap_or(0);
        (get(7), get(9))
    }

    /// Whether `TFT016`'s comparison predicts `mlockall`'s outcome.
    ///
    /// It compares `RLIMIT_MEMLOCK` against the **arena**; `mlockall` charges the
    /// process's whole address space. Run against a limit comfortably above a
    /// small arena and the call still fails.
    pub(crate) fn memlock_limit(argv: &[String]) {
        let limits: Vec<u64> = if argv.len() > 2 {
            argv[2..].iter().filter_map(|a| a.parse().ok()).collect()
        } else {
            vec![65_536, 1_048_576, 8_388_608]
        };
        println!("memlock-limit: TFT016 compares the limit against the ARENA; mlockall charges");
        println!("               the whole address space, so the two ask different questions");
        let me = std::env::current_exe().ok();
        for lim in limits {
            for onfault in [false, true] {
                let Some(me) = me.as_ref() else { continue };
                let out = std::process::Command::new(me)
                    .arg("memlock-limit-child")
                    .arg(lim.to_string())
                    .arg(if onfault { "1" } else { "0" })
                    .output();
                match out {
                    Ok(o) => print!("  {}", String::from_utf8_lossy(&o.stdout)),
                    Err(e) => println!("  (child did not start: {e})"),
                }
            }
        }
        println!(
            "  a limit above the arena and below the address space is where TFT016 is \
             silent and the call it names fails"
        );
    }

    /// One `setrlimit` + `mlockall` measurement, in a process of its own because
    /// both are irreversible for the caller.
    pub(crate) fn memlock_limit_child(argv: &[String]) {
        let lim: u64 = argv.get(2).and_then(|a| a.parse().ok()).unwrap_or(0);
        let onfault = argv.get(3).map(String::as_str) == Some("1");
        let rl = libc::rlimit {
            rlim_cur: lim,
            rlim_max: lim,
        };
        // SAFETY: `rl` is a fully initialised `rlimit` and lives across the call.
        let rc = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rl) };
        if rc != 0 {
            println!("lim={lim} setrlimit failed: {}", last_error());
            return;
        }
        let mut flags = libc::MCL_CURRENT | libc::MCL_FUTURE;
        if onfault {
            flags |= libc::MCL_ONFAULT;
        }
        // SAFETY: no arguments beyond the flag word.
        let rc = unsafe { libc::mlockall(flags) };
        let err = if rc == 0 { "ok".into() } else { last_error() };
        println!(
            "lim={lim} onfault={} mlockall rc={rc} ({err})",
            u8::from(onfault)
        );
    }

    /// The organic half of the reclaim question: does a kernel under real
    /// pressure choose these folios?
    ///
    /// `file` swaps the `memfd` for a file-backed mapping, which is the
    /// **positive control**: it has a backing store, so if the instrument can
    /// see organic PTE teardown at all it sees it there. Read that arm before
    /// believing the shmem one.
    pub(crate) fn pressure(file: bool) {
        let (p, kind) = if file {
            (file_arena(), "file-backed")
        } else {
            (arena(true), "memfd (shmem)")
        };
        let base = p.cast::<u8>();
        for i in (0..LEN).step_by(PAGE) {
            // SAFETY: `i < LEN`, mapping is writable and populated.
            unsafe { std::ptr::write_volatile(base.add(i), ((i / PAGE) & 0xff) as u8) };
        }
        eprintln!("pressure [{kind}]: populated Rss={} kB", rss_kb(p));
        let mut grown = 0usize;
        for step in 0..24u8 {
            // SAFETY: an ordinary anonymous mapping; `MAP_FAILED` is checked.
            let a = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    4 << 20,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if a == libc::MAP_FAILED {
                eprintln!("  anon mmap failed at {grown} MiB");
                break;
            }
            // SAFETY: `a` is the 4 MiB mapping just returned.
            unsafe { std::ptr::write_bytes(a.cast::<u8>(), step.wrapping_add(1), 4 << 20) };
            grown += 4;
            eprintln!("  anon={grown} MiB  arena Rss={} kB", rss_kb(p));
        }
        let mut corrupt = 0u64;
        for i in (0..LEN).step_by(PAGE) {
            // SAFETY: as above.
            if unsafe { std::ptr::read_volatile(base.add(i)) } != ((i / PAGE) & 0xff) as u8 {
                corrupt += 1;
            }
        }
        println!(
            "pressure [{kind}]: SURVIVED  final Rss={} kB  corrupted={corrupt}",
            rss_kb(p)
        );
        println!(
            "  a run that prints nothing was SIGKILLed: the kernel OOM-killed rather \
             than reclaiming, which is the answer, not a failed run"
        );
    }

    /// A file-backed mapping of the same size, as `pressure`'s positive control.
    fn file_arena() -> *mut libc::c_void {
        let path = std::env::temp_dir().join(format!("mlock_probe-{}.bin", std::process::id()));
        let f = match std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("mlock_probe: cannot create {}: {e}", path.display());
                std::process::exit(2);
            }
        };
        if let Err(e) = f.set_len(LEN as u64) {
            eprintln!("mlock_probe: cannot size {}: {e}", path.display());
            std::process::exit(2);
        }
        // The file is unlinked immediately: the mapping keeps it alive, and a
        // run that is OOM-killed leaves nothing behind.
        let _ = std::fs::remove_file(&path);
        use std::os::fd::AsRawFd;
        // SAFETY: `f` is open for the duration of the call and the mapping keeps
        // the description alive afterwards.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                LEN,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                f.as_raw_fd(),
                0,
            )
        };
        assert!(p != libc::MAP_FAILED, "mmap: {}", last_error());
        // SAFETY: `p`/`LEN` is the mapping just created.
        let rc = unsafe { libc::madvise(p, LEN, libc::MADV_POPULATE_WRITE) };
        assert!(rc == 0, "MADV_POPULATE_WRITE: {}", last_error());
        p
    }
}
