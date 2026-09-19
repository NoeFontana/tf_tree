//! The structural checks `docs/decisions/0059` step 1(b) holds every arena
//! error's `Display` to, shared by the rendering tests beside `ShmError`,
//! `FrozenError` and `LayoutError`.
//!
//! **Structure, never a sentence.** `docs/API.md` R5 is NORMATIVE that message
//! text is not a compatibility promise, so nothing here compares a rendering
//! with a literal. What it does pin is decision 2's rules.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// `0059` decision 2(e): at most this many bytes, with every carried integer at
/// its type's maximum. Derived from `TFT_MESSAGE_LEN` in that record's
/// *Rationale*, not measured against a failure, so moving it is one constant.
pub(crate) const MAX_RENDERED_BYTES: usize = 120;

/// The variant's name as `Debug` spells it: everything before the first `(` or
/// ` {`, or the whole string for a unit variant. Computed rather than written
/// down, so a test cannot agree with a typo in the `Display` it checks.
pub(crate) fn variant_name(debug: &str) -> &str {
    let cut = [debug.find('('), debug.find(" {")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(debug.len());
    &debug[..cut]
}

/// Assert decision 2's rules over one rendering.
///
/// `debug` is the `{:?}` of the value that was rendered, `name` the innermost
/// variant's name, and `numbers` every carried integer in decision 2(a)'s
/// spelling, repeated once per field that carries it: two fields at the same
/// maximum must appear twice, so a rendering that drops one of them still fails.
pub(crate) fn assert_structure(shown: &str, debug: &str, name: &str, numbers: &[String]) {
    assert!(!shown.is_empty(), "{debug} renders as nothing");
    assert_ne!(shown, debug, "{debug} renders as its Debug");
    assert!(
        !shown.contains('{') && !shown.contains('}'),
        "{debug} renders with a brace, which is a struct dump: {shown:?}"
    );
    assert!(shown.is_ascii(), "{debug} renders non-ASCII: {shown:?}");
    assert!(
        shown.len() <= MAX_RENDERED_BYTES,
        "{debug} renders as {} bytes, past {MAX_RENDERED_BYTES}: {shown:?}",
        shown.len()
    );
    let key = format!("({name})");
    assert!(
        shown.ends_with(&key),
        "{debug} does not end with its search key {key}: {shown:?}"
    );
    let mut seen: Vec<&String> = Vec::new();
    for n in numbers {
        if seen.contains(&n) {
            continue;
        }
        seen.push(n);
        let want = numbers.iter().filter(|m| *m == n).count();
        let got = shown.matches(n.as_str()).count();
        assert!(
            got >= want,
            "{debug} carries {n} {want} time(s) and renders it {got}: {shown:?}"
        );
    }
}
