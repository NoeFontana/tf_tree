//! Host facts behind `TFT016` — `docs/PHASE5.md` §6.
//!
//! Two properties of the machine that change how an arena behaves and that
//! nothing in the arena can see:
//!
//! * **Transparent huge pages.** §2.3 aligns a frozen arena to 2 MiB precisely
//!   so the mapping is THP-eligible, and cites the arithmetic: a 115 MB index on
//!   4 KiB pages needs ~28 000 TLB entries and 55 on 2 MiB pages. On a host with
//!   THP set to `never` that alignment buys nothing, and the p99 lookup latency
//!   an operator measures will not match the one in the benchmark report.
//! * **`RLIMIT_MEMLOCK`.** Locking the arena is how a hard-real-time consumer
//!   keeps a page fault out of its control loop. A limit below the arena size
//!   means `mlock` will fail, and it fails at the worst possible moment —
//!   during the first deadline miss, when somebody is trying to work out why.
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
    /// Transparent huge pages.
    pub thp: Thp,
    /// Soft `RLIMIT_MEMLOCK`.
    pub memlock: MemLock,
}

/// Read both facts. Linux-only; `TFT016` is skipped elsewhere.
#[cfg(target_os = "linux")]
#[must_use]
pub fn probe() -> HostFacts {
    let thp = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled")
        .map_or(Thp::Unknown, |s| parse_thp(&s));
    let memlock = std::fs::read_to_string("/proc/self/limits")
        .map_or(MemLock::Unknown, |s| parse_memlock(&s));
    HostFacts { thp, memlock }
}

/// Parse `/sys/kernel/mm/transparent_hugepage/enabled`.
///
/// The file lists every policy and brackets the active one:
/// `always [madvise] never`. Matching on the *bracketed* token rather than on
/// `contains("never")` is the whole job — every host's file contains all three
/// words.
#[must_use]
pub fn parse_thp(s: &str) -> Thp {
    let Some(open) = s.find('[') else {
        return Thp::Unknown;
    };
    let rest = &s[open + 1..];
    let Some(close) = rest.find(']') else {
        return Thp::Unknown;
    };
    match &rest[..close] {
        "always" => Thp::Always,
        "madvise" => Thp::Madvise,
        "never" => Thp::Never,
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
