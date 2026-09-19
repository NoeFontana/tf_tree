//! Host facts behind `TFT016` — `docs/PHASE5.md` §6.
//!
//! Machine properties that change how an arena behaves and that nothing in the
//! arena can see: THP for anonymous mappings (§2.3's 2 MiB alignment buys
//! nothing under `never`), THP for *shmem* mappings (a separate knob; the one
//! that governs the live arena), and `RLIMIT_MEMLOCK`.
//!
//! `tf_tree` never calls `mlock`; the limit is reported for the consumer to act
//! on (`docs/decisions/0049-the-flag-that-prefaults-the-arena.md`). The check
//! compares the limit against the *arena* only, so silence is not a clearance:
//! `mlockall` charges the whole address space.
//!
//! `/proc/self/limits` is read instead of `getrlimit(2)` because the crate is
//! `#![forbid(unsafe_code)]` with no `libc` (`docs/decisions/0007`). Both parsers
//! are pure functions over `&str`.

/// The kernel's transparent-huge-page policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Thp {
    /// `always` — every eligible mapping gets huge pages.
    Always,
    /// `madvise` — only mappings that asked (§2.4).
    Madvise,
    /// `never` — the 2 MiB alignment buys nothing on this host.
    Never,
    /// The file was absent or in a shape this does not recognise.
    Unknown,
}

/// The kernel's THP policy **for shmem mappings** — a different sysfs knob
/// (`shmem_enabled`, stock default `never`) from [`Thp`], and the one that
/// governs the live arena's `MAP_SHARED` `memfd`. Reading only [`Thp`] reports a
/// host healthy while `MADV_HUGEPAGE` is a silent no-op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShmemThp {
    /// `always` — every shmem mapping large enough gets huge pages.
    Always,
    /// `within_size` — behaves like [`ShmemThp::Advise`] for a whole-mapped arena.
    WithinSize,
    /// `advise` — only mappings that asked; `MADV_HUGEPAGE` is honoured.
    Advise,
    /// `never` — `MADV_HUGEPAGE` does nothing. The stock default.
    Never,
    /// `deny` — as `never`, and refuses even where it would otherwise apply.
    Deny,
    /// `force` — huge pages everywhere, ignoring the advice.
    Force,
    /// The file was absent (no `CONFIG_TRANSPARENT_HUGEPAGE`) or unrecognised.
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

    /// The policy as the kernel spells it; round-trips with [`parse_shmem_thp`].
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
    /// THP for **anonymous** mappings (the frozen `.tft` path).
    pub thp: Thp,
    /// THP for **shmem** mappings (the live arena's `memfd`); see [`ShmemThp`].
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
/// Separate from [`parse_thp`]: six policies, and sharing it would map `advise`
/// and `within_size` to `Unknown`.
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

/// Parse `/sys/kernel/mm/transparent_hugepage/enabled` (`always [madvise] never`).
/// Matches the *bracketed* token; every file contains all three words.
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
/// Parse the `Max locked memory` row of `/proc/self/limits`.
///
/// The limit names contain spaces, so the prefix is stripped before splitting.
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

    /// The active policy is the bracketed one.
    ///
    /// Mutant: `s.contains("never")` ⇒ `[always]` and `[madvise]` report `Never`.
    #[test]
    fn thp_parsing_reads_the_bracketed_policy_not_the_menu() {
        assert_eq!(parse_thp("[always] madvise never\n"), Thp::Always);
        assert_eq!(parse_thp("always [madvise] never\n"), Thp::Madvise);
        assert_eq!(parse_thp("always madvise [never]\n"), Thp::Never);
        assert_eq!(parse_thp("always madvise never\n"), Thp::Unknown);
        assert_eq!(parse_thp(""), Thp::Unknown);
        assert_eq!(parse_thp("[bogus]"), Thp::Unknown);
    }

    /// `shmem_enabled` has six policies, not the three of `enabled`.
    ///
    /// Mutant: route it through `parse_thp` ⇒ `advise`/`within_size` become `Unknown`.
    #[test]
    fn shmem_thp_parsing_covers_all_six_policies_not_the_three_of_enabled() {
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

        // Only these four let MADV_HUGEPAGE do anything.
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

        // `name()` round-trips: operators can write it back into sysfs.
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

    /// The limit names contain spaces, so a whitespace split reads the wrong column.
    ///
    /// Mutant: strip `"Max locked"` instead of the full name ⇒ `Unknown`.
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
