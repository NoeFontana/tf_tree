//! Frame-name normalization — `docs/PHASE4.md` §5.6.
//!
//! Strips a single leading `/` (warn once per distinct frame), rejects empty
//! names, passes UTF-8 through unchanged, and applies `tf_prefix`.

use std::collections::BTreeSet;

/// Why a name was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    /// The name was empty, or only a slash.
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
    /// Whether this input is new (warn-once); the caller logs.
    pub first_sight: bool,
}

/// Cap on the warn-once set.
const MAX_TRACKED_NAMES: usize = 8192;

/// Normalizes frame names and remembers which ones it has warned about.
#[derive(Debug, Default)]
pub struct NameNormalizer {
    prefix: Option<String>,
    seen: BTreeSet<String>,
    remaps: Vec<(String, String)>,
    stripped: u64,
}

impl NameNormalizer {
    /// A normalizer with no `tf_prefix`.
    #[must_use]
    pub fn new() -> NameNormalizer {
        NameNormalizer::default()
    }

    /// A normalizer that prefixes every frame with `prefix`. An empty or
    /// whitespace-only prefix means no prefix.
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
        // Bounded: the publisher chooses the string.
        let first_sight = if self.seen.contains(raw) || self.seen.len() >= MAX_TRACKED_NAMES {
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

    /// Every remap applied so far, as `(raw, normalized)` (§5.6).
    #[must_use]
    pub fn remaps(&self) -> &[(String, String)] {
        &self.remaps
    }

    /// How many names arrived with a leading slash (§5.9).
    #[must_use]
    pub fn stripped_count(&self) -> u64 {
        self.stripped
    }

    /// Add `n` to the stripped-slash count (for `Ingest::resolve`'s cache hits).
    pub(crate) fn note_stripped(&mut self, n: u64) {
        self.stripped += n;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    /// One leading slash, and only one.
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
        assert_eq!(n.normalize("/robot/base").unwrap().name, "robot/base");
    }

    /// The warning fires once per distinct frame, not once per message.
    #[test]
    fn a_repeated_frame_is_reported_once() {
        let mut n = NameNormalizer::new();
        assert!(n.normalize("/base").unwrap().first_sight);
        for _ in 0..1000 {
            assert!(!n.normalize("/base").unwrap().first_sight);
        }
        assert!(n.normalize("/odom").unwrap().first_sight);
        assert_eq!(n.stripped_count(), 1002, "the count keeps every occurrence");
        assert_eq!(n.remaps().len(), 2, "the table keeps one row per frame");
    }

    /// Case and Unicode are left alone: frame names are identifiers.
    #[test]
    fn case_and_unicode_pass_through_unchanged() {
        let mut n = NameNormalizer::new();
        assert_eq!(n.normalize("Base_Link").unwrap().name, "Base_Link");
        assert_eq!(n.normalize("base_link").unwrap().name, "base_link");
        assert_eq!(n.normalize("caméra").unwrap().name, "caméra");
        let decomposed = "came\u{301}ra"; // e + combining acute
        assert_eq!(n.normalize(decomposed).unwrap().name, decomposed);
        assert_ne!(
            n.normalize("caméra").unwrap().name,
            n.normalize(decomposed).unwrap().name,
            "Unicode normalization would merge two distinct identifiers"
        );
    }

    /// Empty and a bare slash are refused.
    #[test]
    fn empty_and_bare_slash_are_refused() {
        let mut n = NameNormalizer::new();
        assert_eq!(n.normalize(""), Err(NameError::Empty));
        assert_eq!(n.normalize("/"), Err(NameError::Empty));
        assert_eq!(n.remaps().len(), 0);
        assert_eq!(n.stripped_count(), 0);
    }

    /// `tf_prefix` is applied and recorded.
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

    /// A trailing slash on the prefix does not double up; an empty prefix is no remap.
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
