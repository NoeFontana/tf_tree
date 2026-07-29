//! Frame-name normalization — `docs/PHASE4.md` §5.6.
//!
//! # Four rules, and the reasoning behind the two that look arbitrary
//!
//! 1. **Strip a single leading `/`** (ROS 1 legacy), and **warn once per
//!    distinct frame** — not once per message, which at 1 kHz across twenty
//!    edges is twenty thousand identical lines a second.
//! 2. **Reject empty names.**
//! 3. **Otherwise pass UTF-8 through unchanged.** No case folding, no Unicode
//!    normalization. §5.6 is explicit: frame names are *identifiers*, and two
//!    frames differing only by case are two frames. Folding them would merge a
//!    typo'd frame into a real one and produce a transform tree that looks
//!    correct and is not.
//! 4. **Apply `tf_prefix` if configured, and log the resulting table at
//!    startup.** A silent remap is worse than no remap: it makes the arena's
//!    frame names differ from the ones in every launch file and RViz config on
//!    the robot, with nothing anywhere saying so.
//!
//! *A single* leading slash, not all of them: `//base` is not a ROS 1 name with
//! two legacy prefixes, it is a name with a leading slash whose next character
//! happens to be a slash. Stripping greedily would silently merge `/base` and
//! `//base`, which rule 3's reasoning forbids.

use std::collections::BTreeSet;

/// Why a name was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    /// The name was empty, or became empty after stripping.
    ///
    /// A bare `"/"` is the second case, and it is not hypothetical — it is what
    /// a launch file with an unsubstituted variable produces.
    Empty,
}

/// A normalized name and what happened to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Normalized {
    /// The name to use.
    pub name: String,
    /// Whether a leading `/` was stripped.
    pub stripped_slash: bool,
    /// Whether a `tf_prefix` was applied.
    pub prefixed: bool,
    /// Whether this is the **first** time this input has been seen.
    ///
    /// Drives the "warn once per distinct frame" rule. Returning it rather
    /// than logging here keeps the module free of a logging dependency and
    /// leaves the decision with the caller — which matters because the ROS
    /// half logs through `rclcpp` and the tests do not log at all.
    pub first_sight: bool,
}

/// Normalizes frame names and remembers which ones it has warned about.
#[derive(Debug, Default)]
pub struct NameNormalizer {
    prefix: Option<String>,
    /// Inputs already seen, so `first_sight` is per distinct *input*.
    seen: BTreeSet<String>,
    /// The remap table, for the startup log §5.6 requires.
    remaps: Vec<(String, String)>,
    stripped: u64,
}

impl NameNormalizer {
    /// A normalizer with no `tf_prefix`.
    #[must_use]
    pub fn new() -> NameNormalizer {
        NameNormalizer::default()
    }

    /// A normalizer that prefixes every frame with `prefix`.
    ///
    /// An empty or whitespace-only prefix is treated as no prefix, because that
    /// is what an unset launch argument expands to and prefixing every frame
    /// with `""` would otherwise be a silent no-op that still reports itself as
    /// a remap.
    #[must_use]
    pub fn with_prefix(prefix: &str) -> NameNormalizer {
        let p = prefix.trim().trim_end_matches('/');
        NameNormalizer {
            prefix: if p.is_empty() {
                None
            } else {
                Some(p.to_string())
            },
            ..NameNormalizer::default()
        }
    }

    /// Normalize one name.
    ///
    /// # Errors
    ///
    /// [`NameError::Empty`] if the name is empty, or is only a slash.
    pub fn normalize(&mut self, raw: &str) -> Result<Normalized, NameError> {
        if raw.is_empty() {
            return Err(NameError::Empty);
        }
        // **One** slash. See the module docs for why not `trim_start_matches`.
        let (body, stripped_slash) = match raw.strip_prefix('/') {
            Some(rest) => (rest, true),
            None => (raw, false),
        };
        if body.is_empty() {
            return Err(NameError::Empty);
        }
        if stripped_slash {
            self.stripped += 1;
        }

        let (name, prefixed) = match &self.prefix {
            Some(p) => (format!("{p}/{body}"), true),
            None => (body.to_string(), false),
        };
        // `contains` before `insert`, and the order is the whole point:
        // `BTreeSet::insert` needs an owned key whether or not it stores one, so
        // the obvious `self.seen.insert(raw.to_string())` allocates a `String`
        // on **every** sample only to discover the name is already there and
        // drop it again. This runs once per offered transform, which at 1 kHz
        // across twenty edges is twenty thousand pointless allocations a second.
        // The probe borrows (`BTreeSet<String>: Borrow<str>`) and allocates
        // nothing; only a genuinely new name pays.
        let first_sight = if self.seen.contains(raw) {
            false
        } else {
            self.seen.insert(raw.to_string());
            true
        };
        if first_sight && (stripped_slash || prefixed) {
            self.remaps.push((raw.to_string(), name.clone()));
        }
        Ok(Normalized {
            name,
            stripped_slash,
            prefixed,
            first_sight,
        })
    }

    /// Every remap applied so far, as `(raw, normalized)`.
    ///
    /// §5.6 requires this to be logged at startup. It is accumulated rather
    /// than computed up front because the set of frames is not known until they
    /// arrive — so "at startup" in practice means "as each is first seen", and
    /// this is what a caller prints.
    #[must_use]
    pub fn remaps(&self) -> &[(String, String)] {
        &self.remaps
    }

    /// How many names arrived with a leading slash (§5.9).
    #[must_use]
    pub fn stripped_count(&self) -> u64 {
        self.stripped
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// **One leading slash, and only one.**
    ///
    /// Mutant: `trim_start_matches('/')` ⇒ `//base` and `/base` both normalize
    /// to `base`, silently merging two distinct frames. That is the same class
    /// of error as case folding, which §5.6 forbids by name.
    #[test]
    fn exactly_one_leading_slash_is_stripped() {
        let mut n = NameNormalizer::new();
        assert_eq!(n.normalize("/base_link").unwrap().name, "base_link");
        assert_eq!(n.normalize("base_link").unwrap().name, "base_link");
        assert_eq!(
            n.normalize("//base_link").unwrap().name,
            "/base_link",
            "the second slash is part of the name, not a second legacy prefix"
        );
        // Interior slashes are untouched — they are namespaces, not prefixes.
        assert_eq!(n.normalize("/robot/base").unwrap().name, "robot/base");
    }

    /// **The warning fires once per distinct frame, not once per message.**
    ///
    /// At 1 kHz across twenty edges, per-message would be twenty thousand
    /// identical lines a second, which is a log nobody can read for any other
    /// reason.
    ///
    /// Mutant: always report `first_sight: true` ⇒ the caller warns per
    /// message and this fails on the second call.
    #[test]
    fn a_repeated_frame_is_reported_once() {
        let mut n = NameNormalizer::new();
        assert!(n.normalize("/base").unwrap().first_sight);
        for _ in 0..1000 {
            assert!(!n.normalize("/base").unwrap().first_sight);
        }
        // A *different* frame is still worth a warning.
        assert!(n.normalize("/odom").unwrap().first_sight);
        assert_eq!(n.stripped_count(), 1002, "the count keeps every occurrence");
        assert_eq!(n.remaps().len(), 2, "the table keeps one row per frame");
    }

    /// **Case and Unicode are left alone**, because frame names are
    /// identifiers.
    ///
    /// Folding `base_link` and `Base_Link` together would merge a typo'd frame
    /// into a real one, and the resulting tree would look correct.
    #[test]
    fn case_and_unicode_pass_through_unchanged() {
        let mut n = NameNormalizer::new();
        assert_eq!(n.normalize("Base_Link").unwrap().name, "Base_Link");
        assert_eq!(n.normalize("base_link").unwrap().name, "base_link");
        // Two visually similar Unicode names that NFC/NFKC would merge.
        assert_eq!(n.normalize("caméra").unwrap().name, "caméra");
        let decomposed = "came\u{301}ra"; // e + combining acute
        assert_eq!(n.normalize(decomposed).unwrap().name, decomposed);
        assert_ne!(
            n.normalize("caméra").unwrap().name,
            n.normalize(decomposed).unwrap().name,
            "Unicode normalization would merge two distinct identifiers"
        );
    }

    /// **Empty is refused, and so is a bare slash** — which is what an
    /// unsubstituted launch variable produces, and is therefore the one that
    /// actually happens.
    #[test]
    fn empty_and_bare_slash_are_refused() {
        let mut n = NameNormalizer::new();
        assert_eq!(n.normalize(""), Err(NameError::Empty));
        assert_eq!(n.normalize("/"), Err(NameError::Empty));
        // A refused name must not enter the remap table or the counters.
        assert_eq!(n.remaps().len(), 0);
        assert_eq!(n.stripped_count(), 0);
    }

    /// **`tf_prefix` is applied and recorded.** A silent remap makes the
    /// arena's names differ from every launch file on the robot.
    #[test]
    fn a_prefix_is_applied_and_appears_in_the_table() {
        let mut n = NameNormalizer::with_prefix("robot1");
        let r = n.normalize("/base_link").unwrap();
        assert_eq!(r.name, "robot1/base_link");
        assert!(r.prefixed && r.stripped_slash);
        assert_eq!(
            n.remaps(),
            &[("/base_link".to_string(), "robot1/base_link".to_string())]
        );
    }

    /// A trailing slash on the prefix must not double up, and an unset launch
    /// argument (which expands to `""`) must not be treated as a remap.
    #[test]
    fn a_degenerate_prefix_is_treated_as_no_prefix() {
        let mut n = NameNormalizer::with_prefix("robot1/");
        assert_eq!(n.normalize("base").unwrap().name, "robot1/base");

        for empty in ["", "   "] {
            let mut n = NameNormalizer::with_prefix(empty);
            let r = n.normalize("base").unwrap();
            assert_eq!(r.name, "base");
            assert!(!r.prefixed, "an unset prefix must not report a remap");
            assert!(n.remaps().is_empty());
        }
    }
}
