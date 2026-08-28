//! Host facts behind `TFT016` — `docs/PHASE5.md` §6.
//!
//! Properties of the machine that change how an arena behaves and that nothing
//! in the arena can see:
//!
//! * **Transparent huge pages, for anonymous mappings.** §2.3 aligns a frozen
//!   arena to 2 MiB precisely so the mapping is THP-eligible, and cites the
//!   arithmetic: a 115 MB index on 4 KiB pages needs ~28 000 TLB entries and 55
//!   on 2 MiB pages. On a host with THP set to `never` that alignment buys
//!   nothing, and the p99 lookup latency an operator measures will not match the
//!   one in the benchmark report.
//! * **Transparent huge pages, for *shmem* mappings** — a **separate** sysfs
//!   knob, and the one that governs the live arena. See [`ShmemThp`]: reading
//!   only the first file reported a host as healthy while `MADV_HUGEPAGE` on the
//!   arena's `MAP_SHARED` `memfd` was a silent no-op, which is the failure
//!   `TFT016` exists to catch.
//! * **`RLIMIT_MEMLOCK`.** Pinning the arena is how a hard-real-time consumer
//!   keeps a page fault out of its control loop. A limit below the arena size
//!   means the pinning fails, and it fails at the worst possible moment — during
//!   the first deadline miss, when somebody is trying to work out why.
//!
//!   **`tf_tree` never calls `mlock`, and this row does not imply that it does.**
//!   `docs/PHASE2.md` §7.4 specifies a `LockPolicy::Locked` that exists nowhere
//!   in this codebase (§0.0 carries the row), and `docs/API.md` §8.3 records why
//!   it is not simply missing work: `MLOCK_ONFAULT`, the flag §7.4 names, does
//!   not prefault, so it adds nothing over §7.1's shipped per-edge
//!   `MADV_POPULATE_*` — and pinning a whole address space is
//!   `mlockall(MCL_CURRENT|MCL_FUTURE)` in the *embedding application*, which is
//!   the only place that can see the `RLIMIT_MEMLOCK` budget it is spending.
//!   So this is a limit reported **for the consumer to act on**, not for us.
//!
//! # Why `/proc/self/limits` rather than `getrlimit`
//!
//! `tf_tree_cli` is `#![forbid(unsafe_code)]` and has no `libc` dependency, so
//! `getrlimit(2)` is not available to it — and `docs/decisions/0007`'s unsafe
//! budget has four boundaries, none of which is "the CLI wanted a syscall".
//! `/proc/self/limits` is the same number, from the same kernel, as text. The
//! only cost is parsing, which is why both parsers here are pure functions over
//! `&str` with their own tests: they can be checked against a captured file
//! rather than against whatever this host happens to be configured as.

/// The kernel's transparent-huge-page policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Thp {
    /// `always` — every eligible mapping gets huge pages.
    Always,
    /// `madvise` — only mappings that asked. §2.4's `MADV_HUGEPAGE` is
    /// meaningful, so this is a perfectly good setting.
    Madvise,
    /// `never` — the 2 MiB alignment buys nothing on this host.
    Never,
    /// The file was absent or in a shape this does not recognise.
    Unknown,
}

/// The kernel's transparent-huge-page policy **for shmem mappings**, which is a
/// different knob from [`Thp`] with a different vocabulary.
///
/// # Why this exists separately, and why reading only [`Thp`] was a defect
///
/// A live tf_tree arena is a sealed `memfd` mapped `MAP_SHARED` — shmem, not
/// anonymous memory — and shmem THP is **not** governed by
/// `transparent_hugepage/enabled`. It is governed by
/// `transparent_hugepage/shmem_enabled`, whose default on a stock distribution
/// is `never`:
///
/// ```text
/// enabled:       always [madvise] never
/// shmem_enabled: always within_size advise [never] deny force
/// ```
///
/// So a host reads as perfectly healthy on `enabled` while
/// `MappedArena`'s `MADV_HUGEPAGE` (`mapped.rs`) is silently a no-op and the
/// arena gets 4 KiB pages. `TFT016` reported that host as passing, which is the
/// one thing a diagnostic must not do.
///
/// The frozen `.tft` path is a file mapping and is governed by neither of these
/// two files, which is why [`HostFacts`] reports both settings rather than
/// collapsing them into one verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShmemThp {
    /// `always` — every shmem mapping large enough gets huge pages.
    Always,
    /// `within_size` — huge pages only up to the file's size. An arena is mapped
    /// whole, so this behaves like [`ShmemThp::Advise`] for us.
    WithinSize,
    /// `advise` — only mappings that asked. `MADV_HUGEPAGE` is honoured, which
    /// is what `MappedArena` issues, so this is the setting that makes §2.3's
    /// alignment mean something.
    Advise,
    /// `never` — `MADV_HUGEPAGE` on a shmem mapping does nothing. **The stock
    /// default.**
    Never,
    /// `deny` — as `never`, and refuses even where it would otherwise apply.
    Deny,
    /// `force` — huge pages everywhere, ignoring the advice.
    Force,
    /// The file was absent or in a shape this does not recognise. Absent is the
    /// normal reading on a kernel built without `CONFIG_TRANSPARENT_HUGEPAGE`.
    Unknown,
}

impl ShmemThp {
    /// Whether `MADV_HUGEPAGE` on a `MAP_SHARED` `memfd` can be honoured.
    #[must_use]
    pub fn honours_madvise(self) -> bool {
        matches!(
            self,
            ShmemThp::Always | ShmemThp::WithinSize | ShmemThp::Advise | ShmemThp::Force
        )
    }

    /// The policy as the kernel spells it, for a diagnostic that quotes it back.
    ///
    /// Round-trips with [`parse_shmem_thp`], so the string a finding prints is
    /// the string an operator can write into the sysfs file — which is the whole
    /// point of quoting it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ShmemThp::Always => "always",
            ShmemThp::WithinSize => "within_size",
            ShmemThp::Advise => "advise",
            ShmemThp::Never => "never",
            ShmemThp::Deny => "deny",
            ShmemThp::Force => "force",
            ShmemThp::Unknown => "unknown",
        }
    }
}

/// The soft `RLIMIT_MEMLOCK`, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemLock {
    /// No limit; `mlock` of any size is permitted.
    Unlimited,
    /// A byte limit.
    Bytes(u64),
    /// `/proc/self/limits` was absent or unparseable.
    Unknown,
}

/// What the host says about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostFacts {
    /// Transparent huge pages for **anonymous** mappings. Governs the frozen
    /// `.tft` path's eligibility, not the live arena's.
    pub thp: Thp,
    /// Transparent huge pages for **shmem** mappings — the live arena's `memfd`.
    /// See [`ShmemThp`] for why this is a separate knob and why reading only
    /// `thp` reported a broken host as healthy.
    pub shmem_thp: ShmemThp,
    /// Soft `RLIMIT_MEMLOCK`.
    pub memlock: MemLock,
}

/// Read every fact. Linux-only; `TFT016` is skipped elsewhere.
#[cfg(target_os = "linux")]
#[must_use]
pub fn probe() -> HostFacts {
    let thp = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
        .map_or(Thp::Unknown, |s| parse_thp(&s));
    let shmem_thp = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/shmem_enabled")
        .map_or(ShmemThp::Unknown, |s| parse_shmem_thp(&s));
    let memlock = std::fs::read_to_string("/proc/self/limits")
        .map_or(MemLock::Unknown, |s| parse_memlock(&s));
    HostFacts {
        thp,
        shmem_thp,
        memlock,
    }
}

/// Parse `/sys/kernel/mm/transparent_hugepage/shmem_enabled`.
///
/// Same bracketed-token shape as [`parse_thp`], different vocabulary — six
/// policies rather than three — so it cannot share that parser without silently
/// mapping `advise` and `within_size` to [`Thp::Unknown`], which would report
/// "policy unknown" on a correctly configured host.
#[must_use]
pub fn parse_shmem_thp(s: &str) -> ShmemThp {
    match bracketed(s) {
        Some("always") => ShmemThp::Always,
        Some("within_size") => ShmemThp::WithinSize,
        Some("advise") => ShmemThp::Advise,
        Some("never") => ShmemThp::Never,
        Some("deny") => ShmemThp::Deny,
        Some("force") => ShmemThp::Force,
        _ => ShmemThp::Unknown,
    }
}

/// The token between `[` and `]`, which is how both `transparent_hugepage`
/// files mark the active policy.
fn bracketed(s: &str) -> Option<&str> {
    let rest = &s[s.find('[')? + 1..];
    Some(&rest[..rest.find(']')?])
}

/// Parse `/sys/kernel/mm/transparent_hugepage/enabled`.
///
/// The file lists every policy and brackets the active one:
/// `always [madvise] never`. Matching on the *bracketed* token rather than on
/// `contains("never")` is the whole job — every host's file contains all three
/// words.
#[must_use]
pub fn parse_thp(s: &str) -> Thp {
    match bracketed(s) {
        Some("always") => Thp::Always,
        Some("madvise") => Thp::Madvise,
        Some("never") => Thp::Never,
        _ => Thp::Unknown,
    }
}

/// Parse the `Max locked memory` row of `/proc/self/limits`.
///
/// ```text
/// Limit                     Soft Limit           Hard Limit           Units
/// Max locked memory         8388608              8388608              bytes
/// ```
///
/// The limit *names* contain spaces, so the row cannot be split on whitespace
/// and indexed — the prefix has to be stripped first, which is why this is not
/// a one-liner.
#[must_use]
pub fn parse_memlock(s: &str) -> MemLock {
    const NAME: &str = "Max locked memory";
    for line in s.lines() {
        let Some(rest) = line.strip_prefix(NAME) else {
            continue;
        };
        let Some(soft) = rest.split_whitespace().next() else {
            return MemLock::Unknown;
        };
        return match soft {
            "unlimited" => MemLock::Unlimited,
            n => n.parse::<u64>().map_or(MemLock::Unknown, MemLock::Bytes),
        };
    }
    MemLock::Unknown
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// **The active policy is the bracketed one, and every file contains all
    /// three words.**
    ///
    /// Mutant: replace the bracket scan with `s.contains("never")` ⇒ the
    /// `[always]` and `[madvise]` cases both report `Never`, and `TFT016` fires
    /// on every correctly configured host. Applied and confirmed.
    #[test]
    fn thp_parsing_reads_the_bracketed_policy_not_the_menu() {
        assert_eq!(parse_thp("[always] madvise never\n"), Thp::Always);
        assert_eq!(parse_thp("always [madvise] never\n"), Thp::Madvise);
        assert_eq!(parse_thp("always madvise [never]\n"), Thp::Never);
        assert_eq!(parse_thp("always madvise never\n"), Thp::Unknown);
        assert_eq!(parse_thp(""), Thp::Unknown);
        assert_eq!(parse_thp("[bogus]"), Thp::Unknown);
    }

    /// **`shmem_enabled` is a different knob with a different vocabulary**, and
    /// it is the one that governs the live arena.
    ///
    /// A live arena is a sealed `memfd` mapped `MAP_SHARED`, so its huge-page
    /// eligibility comes from `shmem_enabled`, not from `enabled`. This host
    /// reads `always [madvise] never` on the first and
    /// `always within_size advise [never] deny force` on the second — healthy by
    /// the wrong file, and `MADV_HUGEPAGE` a silent no-op by the right one. Both
    /// real strings are pinned below.
    ///
    /// Mutant: route `shmem_enabled` through `parse_thp` ⇒ `advise` and
    /// `within_size` both become `Unknown`, so a correctly configured host is
    /// reported as "policy unknown" instead of passing. Applied and confirmed.
    #[test]
    fn shmem_thp_parsing_covers_all_six_policies_not_the_three_of_enabled() {
        // The two files as this host actually reports them.
        assert_eq!(parse_thp("always [madvise] never\n"), Thp::Madvise);
        assert_eq!(
            parse_shmem_thp("always within_size advise [never] deny force\n"),
            ShmemThp::Never
        );

        for (s, want) in [
            (
                "[always] within_size advise never deny force",
                ShmemThp::Always,
            ),
            (
                "always [within_size] advise never deny force",
                ShmemThp::WithinSize,
            ),
            (
                "always within_size [advise] never deny force",
                ShmemThp::Advise,
            ),
            (
                "always within_size advise never [deny] force",
                ShmemThp::Deny,
            ),
            (
                "always within_size advise never deny [force]",
                ShmemThp::Force,
            ),
            (
                "always within_size advise never deny force",
                ShmemThp::Unknown,
            ),
            ("", ShmemThp::Unknown),
        ] {
            assert_eq!(parse_shmem_thp(s), want, "parsing {s:?}");
        }

        // Only these four let `MappedArena`'s MADV_HUGEPAGE do anything. Getting
        // this set wrong is the whole check: `never` is the stock default, so a
        // predicate that accepted it would restore the defect this test exists
        // to pin.
        for p in [
            ShmemThp::Always,
            ShmemThp::WithinSize,
            ShmemThp::Advise,
            ShmemThp::Force,
        ] {
            assert!(p.honours_madvise(), "{p:?} should honour madvise");
        }
        for p in [ShmemThp::Never, ShmemThp::Deny, ShmemThp::Unknown] {
            assert!(!p.honours_madvise(), "{p:?} must not honour madvise");
        }

        // `name()` must round-trip through the parser: the finding quotes the
        // policy back at the operator, and a string they cannot write into the
        // sysfs file is worse than no string at all.
        for p in [
            ShmemThp::Always,
            ShmemThp::WithinSize,
            ShmemThp::Advise,
            ShmemThp::Never,
            ShmemThp::Deny,
            ShmemThp::Force,
        ] {
            assert_eq!(parse_shmem_thp(&format!("[{}]", p.name())), p);
        }
    }

    /// **The limit names contain spaces**, so a whitespace split and an index
    /// reads the wrong column — and `Max locked memory` sits directly above
    /// `Max address space` in real files, whose value is `unlimited` on most
    /// hosts. Reading the neighbouring row would report no limit at all.
    ///
    /// Mutant: strip the prefix `"Max locked"` instead of the full name ⇒
    /// `"memory"` becomes the first token and the parse returns `Unknown`.
    /// Applied and confirmed.
    #[test]
    fn memlock_parsing_handles_the_multi_word_limit_names() {
        let real = "\
Limit                     Soft Limit           Hard Limit           Units
Max cpu time              unlimited            unlimited            seconds
Max locked memory         8388608              8388608              bytes
Max address space         unlimited            unlimited            bytes
";
        assert_eq!(parse_memlock(real), MemLock::Bytes(8_388_608));
        assert_eq!(
            parse_memlock(
                "Max locked memory         unlimited            unlimited            bytes\n"
            ),
            MemLock::Unlimited
        );
        assert_eq!(parse_memlock("Max cpu time  unlimited\n"), MemLock::Unknown);
        assert_eq!(parse_memlock(""), MemLock::Unknown);
    }
}
