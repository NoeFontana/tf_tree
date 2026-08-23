//! `(pid, start_time, boot_id)` — the identity triple, and the one parser in
//! this crate that is a documented trap.
//!
//! `docs/PHASE2.md` §5.1: since the lock file became authoritative for liveness,
//! none of this is on a correctness-critical path any more. It survives because
//! `doctor` reports it, the takeover path prints it, and
//! [`crate::CreatePolicy::Always`] — §3.4's escape hatch, which that section
//! calls `--force-new` and which no binary exposes as a flag (§0.0, #189) —
//! needs to say *whose* arena is being abandoned. A bare pid is not an identity
//! — pids are recycled, and on an embedded system with a low `pid_max` they
//! recycle fast — so a record that names a pid without its start time names
//! nothing.

use crate::error::{ProcError, ProcParseError};

/// Field 22 of `/proc/<pid>/stat`: the process's start time, in clock ticks
/// since boot.
///
/// # Errors
///
/// [`ProcError::Unreadable`] if the process is gone (the common case, and
/// information rather than a fault), [`ProcError::Parse`] if the line is
/// malformed.
pub fn start_time_of(pid: u32) -> Result<u64, ProcError> {
    let path = format!("/proc/{pid}/stat");
    let raw = std::fs::read_to_string(&path).map_err(|e| ProcError::Unreadable {
        pid,
        raw_os_error: e.raw_os_error().unwrap_or(0),
    })?;
    parse_start_time(&raw).map_err(|cause| ProcError::Parse { pid, cause })
}

/// This process's start time.
///
/// # Errors
///
/// As [`start_time_of`]. `/proc/self/stat` failing to read is close to
/// impossible outside a broken container, but it is not worth an `unwrap`.
pub fn self_start_time() -> Result<u64, ProcError> {
    let raw = std::fs::read_to_string("/proc/self/stat").map_err(|e| ProcError::Unreadable {
        pid: std::process::id(),
        raw_os_error: e.raw_os_error().unwrap_or(0),
    })?;
    parse_start_time(&raw).map_err(|cause| ProcError::Parse {
        pid: std::process::id(),
        cause,
    })
}

/// Parse field 22 out of one `/proc/<pid>/stat` line.
///
/// **NORMATIVE (`docs/PHASE2.md` §5.1).** Field 2 is `comm`, the executable name
/// wrapped in parentheses, and the kernel does not escape it: it may contain
/// spaces *and* parentheses, because it is derived from the binary's name and a
/// process can be named anything. Splitting the whole line on whitespace and
/// taking index 21 therefore reads a *different field* for any process whose
/// name contains `) `, silently and with a plausible-looking number.
///
/// The only safe anchor is the **last** `)` in the line: `comm` is the only
/// parenthesised field, and every field after it is a number or a single
/// character, so nothing past it can contain another `)`. Fields are then
/// counted from there — `raw[rp + 2..]` starts at field 3, so field 22 is
/// `nth(19)`.
///
/// See the `evil_comm_defeats_the_naive_split` test: the naive parse returns
/// field 12's value for a process named `evil) proc`, which is the exact fixture
/// `docs/PHASE2.md` Appendix B specifies.
///
/// # Errors
///
/// [`ProcParseError`] if there is no `)`, fewer than 22 fields, or field 22 is
/// not a decimal integer.
pub fn parse_start_time(raw: &str) -> Result<u64, ProcParseError> {
    let rp = raw.rfind(')').ok_or(ProcParseError::NoClosingParen)?;
    // `rp + 2` skips ") " — the separator between `comm` and the state field.
    // A line that ends at the paren has no fields left, which `get` reports as
    // `TooFewFields` rather than panicking on the slice.
    let tail = raw.get(rp + 2..).ok_or(ProcParseError::TooFewFields)?;
    let field22 = tail
        .split_ascii_whitespace()
        .nth(19)
        .ok_or(ProcParseError::TooFewFields)?;
    field22.parse().map_err(|_| ProcParseError::NotAnInteger)
}

/// The kernel's boot id, as 16 raw bytes.
///
/// The third element of the identity triple. It is what makes a `(pid,
/// start_time)` pair meaningful across a reboot: start times are measured in
/// ticks since boot, so after a restart they collide freely.
///
/// # Errors
///
/// [`ProcError::BootId`] if the file is missing or is not a 36-character UUID.
pub fn boot_id() -> Result<[u8; 16], ProcError> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|_| ProcError::BootId)?;
    parse_uuid(raw.trim()).ok_or(ProcError::BootId)
}

/// `8-4-4-4-12` hex into 16 bytes.
fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    let mut nibbles = s.bytes().filter(|b| *b != b'-');
    for byte in &mut out {
        let hi = hex(nibbles.next()?)?;
        let lo = hex(nibbles.next()?)?;
        *byte = (hi << 4) | lo;
    }
    // Trailing junk means this is not a UUID, and quietly accepting a prefix
    // would make two different boot ids compare equal.
    if nibbles.next().is_some() {
        return None;
    }
    Some(out)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// This process's `comm`, NUL-padded into the 16 bytes an identity record has
/// for it. Diagnostics only.
///
/// **Sixteen since `docs/decisions/0033`, where it was thirty-two, and the
/// narrowing costs no diagnostic text.** The
/// kernel caps `comm` at 15 bytes of content plus its NUL (`TASK_COMM_LEN`), so
/// no name that ever reached this function filled half the old field: a record
/// written by a process whose binary basename is 52 characters used 15 bytes of
/// the 32 and left `47..64` zero. The eight bytes freed at `48..56` are
/// [`crate::Identity::pid_ns_inode`].
///
/// This is a **public signature change on a publishing crate**, taken on the
/// `0.0.x` line where every release may break every other. In-tree there are
/// three callers, and one of them — `tf_tree`'s handshake `name_bytes` — pads
/// back to 32, because the wire's `client_name` is a different 32 that did not
/// move.
#[must_use]
pub fn self_comm() -> [u8; 16] {
    let mut out = [0u8; 16];
    let raw = std::fs::read_to_string("/proc/self/comm").unwrap_or_default();
    let src = raw.trim().as_bytes();
    let n = core::cmp::min(src.len(), out.len());
    out[..n].copy_from_slice(&src[..n]);
    out
}

/// This process's PID namespace, as the `nsfs` inode `/proc/self/ns/pid` names.
///
/// The discriminator [`crate::Identity`] carries so that a `doctor` in another
/// PID namespace can tell *"the recorded pid is not comparable from here"* from
/// *"the recorded process is gone"* — two faults with opposite operator
/// remediations, and until `0033` the same sentence.
///
/// # `readlink`, not `stat`, and never `lstat`
///
/// **NORMATIVE (`docs/decisions/0033` *Decision* 1).** All three candidate
/// reads were measured in four arms, one process per arm doing all three, and
/// `readlink` is the only one correct in every arm:
///
/// | arm | `read_link` | `metadata().ino()` | `symlink_metadata().ino()` |
/// |---|---|---|---|
/// | plain host | `pid:[4026531836]` | `4026531836` | `81341846` **wrong** |
/// | `unshare -U --fork` | `pid:[4026531836]` | `EACCES` | `81340131` **wrong** |
/// | `unshare -U --fork --pid` | `pid:[4026532488]` | `EACCES` | `81340134` **wrong** |
/// | default `docker run` | `pid:[4026532489]` | `4026532489` | a procfs dentry **wrong** |
///
/// `metadata()` fails *loudly* in the two arms with an unmapped **user**
/// namespace — note that a default container has a pid namespace and no user
/// namespace, so reaching for `docker` as the nearest container makes `stat`
/// look correct and it is not. `symlink_metadata()` succeeds in all four and
/// returns the procfs *dentry's* inode rather than the `nsfs` one it points at:
/// a plausible wrong number, which is the same "successful read of the wrong
/// thing" class `0033` rejects its `/proc/<recorded_pid>` probe for.
///
/// # `None`, not an error
///
/// Every caller treats an unreadable `/proc` as *unknown namespace* and carries
/// on: [`crate::Identity::of_self_best_effort`] exists so that a missing `/proc`
/// cannot fail an `open()`, and `doctor` must degrade to the behaviour it had
/// before this field existed rather than to "cannot say" about every slot. An
/// `IpcError` here would have exactly one correct handler at every call site,
/// which is what makes it the wrong return type.
#[must_use]
pub fn self_pid_ns_inode() -> Option<u64> {
    parse_ns_inode(std::fs::read_link("/proc/self/ns/pid").ok()?.to_str()?)
}

/// `pid:[4026531836]` into `4026531836`.
///
/// Strict on both ends for `parse_uuid`'s reason: quietly accepting a prefix,
/// a suffix or another namespace type's link would make two different
/// namespaces compare equal, and the only thing this number is ever used for is
/// a comparison.
fn parse_ns_inode(link: &str) -> Option<u64> {
    let inner = link.strip_prefix("pid:[")?.strip_suffix(']')?;
    // `u64::from_str` accepts a leading `+`; the kernel never writes one, so a
    // link that carries one is not a link this wrote.
    if inner.is_empty() || !inner.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    inner.parse().ok()
}

/// The pid this process's `/proc` calls it — `readlink("/proc/self")`.
///
/// Half of the second guard `0033` *Decision* 4 adds: it equals
/// [`std::process::id`] exactly when `/proc` describes the caller's **own** pid
/// namespace, and differs when it does not — a bare `unshare -U --fork --pid`
/// that never remounted `/proc`, where every pid in `/proc` is drawn from the
/// parent numbering while `getpid()` is drawn from the child's. On that
/// disagreement no pid written by any process in this file is resolvable here,
/// **including the caller's own** — which is the failure `0033` measured, a
/// `doctor` reporting its own participant slot as a fork inheritor.
///
/// **Compare it in one process or not at all.** The first attempt to measure
/// this read `$(readlink /proc/self)` from a shell and compared it against
/// `$$`; the command substitution forks, so the two halves were about two
/// processes and it disagreed in a container too. That is the same
/// read-of-the-wrong-process shape `0033` rejects, arriving in the experiment
/// instead of the code — hence a function that reads one number and a caller
/// that already holds the other.
///
/// `None` on any failure, and the caller's rule is `0033`'s failed-read rule:
/// degrade to today's behaviour, never to "cannot say" about everything.
#[must_use]
pub fn proc_self_pid() -> Option<u32> {
    std::fs::read_link("/proc/self")
        .ok()?
        .to_str()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The NORMATIVE fixture from `docs/PHASE2.md` Appendix B.
    ///
    /// `comm` is `evil) proc`, so the line contains two `)` and one of them is
    /// inside a field. Field 22 is 13. The naive whitespace split returns 12 —
    /// field 12's value — with no error and no way to notice.
    const EVIL: &str =
        "1234 (evil) proc) S 1 1234 1234 0 -1 4194304 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16";

    #[test]
    fn evil_comm_defeats_the_naive_split() {
        let naive: u64 = EVIL
            .split_ascii_whitespace()
            .nth(21)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            naive, 12,
            "the naive parse should return the wrong field 12"
        );

        let robust = parse_start_time(EVIL).unwrap();
        assert_eq!(robust, 13, "rfind(')') must find field 22");

        assert_ne!(
            naive, robust,
            "if these ever agree the fixture stopped testing anything"
        );
    }

    #[test]
    fn ordinary_comm_parses() {
        // A boring name, so the naive parse happens to agree — which is exactly
        // why the trap survives code review.
        let line = "42 (bash) S 1 42 42 0 -1 4194304 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16";
        assert_eq!(parse_start_time(line).unwrap(), 13);
        let naive: u64 = line
            .split_ascii_whitespace()
            .nth(21)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(naive, 13);
    }

    #[test]
    fn comm_containing_only_a_paren() {
        // `comm` is `((`, i.e. the last `)` is still the closing one.
        let line = "7 ((() S 1 7 7 0 -1 0 1 2 3 4 5 6 7 8 9 10 11 12 13";
        assert_eq!(parse_start_time(line).unwrap(), 13);
    }

    #[test]
    fn malformed_lines_are_errors_not_panics() {
        assert_eq!(
            parse_start_time("no parens here"),
            Err(ProcParseError::NoClosingParen)
        );
        assert_eq!(
            parse_start_time("1 (x) S 1 2 3"),
            Err(ProcParseError::TooFewFields)
        );
        assert_eq!(parse_start_time("1 (x)"), Err(ProcParseError::TooFewFields));
        let not_a_number = "1 (x) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 what 21 22";
        assert_eq!(
            parse_start_time(not_a_number),
            Err(ProcParseError::NotAnInteger)
        );
    }

    #[test]
    fn self_identity_is_readable_and_stable() {
        let a = self_start_time().unwrap();
        let b = start_time_of(std::process::id()).unwrap();
        assert_eq!(a, b, "/proc/self/stat and /proc/<pid>/stat must agree");
        assert!(a > 0, "a live process has a nonzero start time");

        let id = boot_id().unwrap();
        assert_ne!(id, [0u8; 16], "boot_id should not be all zeroes");
        assert_eq!(id, boot_id().unwrap(), "boot_id must be stable");
    }

    #[test]
    fn dead_pids_report_unreadable() {
        // pid 0 never has a /proc entry.
        let err = start_time_of(0).unwrap_err();
        assert!(matches!(err, ProcError::Unreadable { pid: 0, .. }));
    }

    #[test]
    fn uuid_parsing_rejects_near_misses() {
        assert!(parse_uuid("0123456789abcdef0123456789abcdef").is_some());
        assert!(parse_uuid("01234567-89ab-cdef-0123-456789abcdef").is_some());
        assert!(parse_uuid("01234567-89ab-cdef-0123-456789abcde").is_none());
        assert!(parse_uuid("01234567-89ab-cdef-0123-456789abcdef0").is_none());
        assert!(parse_uuid("zzzz4567-89ab-cdef-0123-456789abcdef").is_none());
    }

    /// The two link texts measured in `docs/decisions/0033` *Decision* 1, and
    /// the near misses that must not parse.
    ///
    /// A namespace inode is only ever *compared*, so a parser that accepts a
    /// prefix, a suffix, or another namespace type's link makes two different
    /// namespaces read equal — `parse_uuid`'s trap, one file down.
    #[test]
    fn ns_link_parsing_rejects_near_misses() {
        assert_eq!(parse_ns_inode("pid:[4026531836]"), Some(4_026_531_836));
        assert_eq!(parse_ns_inode("pid:[4026532488]"), Some(4_026_532_488));
        // Another namespace type's link. `/proc/self/ns/` holds eight of these
        // and the numbers are drawn from one allocator, so accepting the prefix
        // would compare a *user* namespace against a pid namespace.
        assert_eq!(parse_ns_inode("user:[4026531837]"), None);
        assert_eq!(parse_ns_inode("mnt:[4026531840]"), None);
        // Truncation in either direction.
        assert_eq!(parse_ns_inode("pid:[4026531836"), None);
        assert_eq!(parse_ns_inode("pid:4026531836]"), None);
        assert_eq!(parse_ns_inode("pid:[]"), None);
        // `u64::from_str` would take this one; the kernel never writes it.
        assert_eq!(parse_ns_inode("pid:[+4026531836]"), None);
        assert_eq!(parse_ns_inode("pid:[ 4026531836 ]"), None);
        assert_eq!(parse_ns_inode(""), None);
    }

    /// The two `/proc` reads `0033` adds, on the host this suite runs on.
    ///
    /// Neither can assert a *value* — an nsfs inum is allocated per namespace
    /// and a test cannot know which — so what is pinned is the shape: the
    /// namespace read parses, and `/proc/self` agrees with `getpid()` on any
    /// host whose `/proc` is its own. The disagreeing arm is not arrangeable
    /// from inside one process, which is why `0033` stages it as arm D of a
    /// subprocess test rather than here.
    #[test]
    fn the_namespace_reads_answer_about_this_process() {
        let ino = self_pid_ns_inode().expect("/proc/self/ns/pid is readable here");
        assert_ne!(ino, 0, "zero is the record's `unknown namespace` marker");
        assert_eq!(
            Some(ino),
            self_pid_ns_inode(),
            "a namespace inode must not change under a process"
        );
        assert_eq!(
            proc_self_pid(),
            Some(std::process::id()),
            "`/proc` describes this process's own pid namespace"
        );
    }
}
