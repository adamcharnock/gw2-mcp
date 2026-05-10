//! Validated slug for `BuildCatalog::fetch`.
//!
//! Catalog adapters (`catalog_snowcrows`, `catalog_discretize`) interpolate
//! the slug straight into a URL path. An LLM (or hostile caller) might pass
//! `../../../etc/passwd`, a slug containing newlines that breaks
//! `url::Url::parse`, or anything else that turns a transport bug into a
//! security vulnerability. Anything entering the URL path must come from
//! this newtype.

use super::error::DomainError;

/// A validated catalog build slug. Constructed via [`BuildSlug::new`];
/// guarantees the contents match `^[a-z0-9_\-/]+$`, contain no `..`, and
/// are no longer than [`BuildSlug::MAX_LEN`] bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BuildSlug(String);

impl BuildSlug {
    /// Slugs longer than this are rejected outright. Real catalog slugs
    /// are well under 100 chars (e.g. `guardian/power-dragonhunter` →
    /// 27 chars; snowcrows raid slugs are similar).
    pub const MAX_LEN: usize = 256;

    /// Validate and wrap. Trims surrounding whitespace before checking;
    /// rejects empty, oversized, traversal-like, or character-class-violating
    /// inputs.
    pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
        let trimmed = raw.into().trim().to_owned();
        if trimmed.is_empty() {
            return Err(DomainError::BuildSlugInvalid {
                got: trimmed,
                reason: "empty",
                max: Self::MAX_LEN,
            });
        }
        if trimmed.len() > Self::MAX_LEN {
            // Don't echo the whole oversized input; truncate to keep the
            // error message bounded.
            let preview: String = trimmed.chars().take(32).collect();
            return Err(DomainError::BuildSlugInvalid {
                got: format!("{preview}… ({} chars)", trimmed.len()),
                reason: "too long",
                max: Self::MAX_LEN,
            });
        }
        // No path-traversal: reject any component matching `..`.
        if trimmed
            .split('/')
            .any(|seg| seg == ".." || seg == "." || seg.is_empty())
        {
            return Err(DomainError::BuildSlugInvalid {
                got: trimmed,
                reason: "contains `..`, `.`, or an empty segment",
                max: Self::MAX_LEN,
            });
        }
        // Character class: ^[a-z0-9_\-/]+$. We do this by hand to avoid
        // pulling in the regex crate for one check.
        if !trimmed.bytes().all(is_slug_byte) {
            return Err(DomainError::BuildSlugInvalid {
                got: trimmed,
                reason: "contains characters outside [a-z0-9_-/]",
                max: Self::MAX_LEN,
            });
        }
        Ok(Self(trimmed))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BuildSlug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

const fn is_slug_byte(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_canonical_slug() {
        let s = BuildSlug::new("guardian/power-dragonhunter").unwrap();
        assert_eq!(s.as_str(), "guardian/power-dragonhunter");
    }

    #[test]
    fn accepts_multi_segment_slug() {
        let s = BuildSlug::new("raids/guardian/heal_firebrand").unwrap();
        assert_eq!(s.as_str(), "raids/guardian/heal_firebrand");
    }

    #[test]
    fn accepts_underscore_and_dash() {
        BuildSlug::new("foo_bar-baz/qux-one_two").unwrap();
    }

    #[test]
    fn trims_surrounding_whitespace() {
        let s = BuildSlug::new("  guardian/foo  ").unwrap();
        assert_eq!(s.as_str(), "guardian/foo");
    }

    #[test]
    fn rejects_empty() {
        let err = BuildSlug::new("").unwrap_err();
        assert!(matches!(err, DomainError::BuildSlugInvalid { .. }));
    }

    #[test]
    fn rejects_traversal() {
        let err = BuildSlug::new("../etc/passwd").unwrap_err();
        assert!(matches!(err, DomainError::BuildSlugInvalid { .. }));
        assert!(BuildSlug::new("foo/../bar").is_err());
        assert!(BuildSlug::new("..").is_err());
    }

    #[test]
    fn rejects_dot_segment() {
        assert!(BuildSlug::new("./foo").is_err());
        assert!(BuildSlug::new("foo/./bar").is_err());
    }

    #[test]
    fn rejects_empty_segment() {
        assert!(BuildSlug::new("/foo").is_err());
        assert!(BuildSlug::new("foo//bar").is_err());
        assert!(BuildSlug::new("foo/").is_err());
    }

    #[test]
    fn rejects_uppercase() {
        assert!(BuildSlug::new("Guardian/Foo").is_err());
    }

    #[test]
    fn rejects_newline() {
        // The reason this newtype exists: a newline in the path arg would
        // explode `url::Url::parse` with a confusing transport error.
        // Trailing whitespace is trimmed before validation, so test the
        // dangerous middle position.
        assert!(BuildSlug::new("guardian\nfoo").is_err());
        assert!(BuildSlug::new("guardian/foo\nbar").is_err());
        assert!(BuildSlug::new("guardian\rfoo").is_err());
    }

    #[test]
    fn rejects_special_characters() {
        assert!(BuildSlug::new("guardian/foo bar").is_err()); // space
        assert!(BuildSlug::new("guardian/foo%20bar").is_err()); // %
        assert!(BuildSlug::new("guardian/?bar").is_err());
        assert!(BuildSlug::new("guardian/#bar").is_err());
        assert!(BuildSlug::new("guardian/.bar").is_err()); // leading dot in segment
        assert!(BuildSlug::new("guardian/bar.html").is_err());
    }

    #[test]
    fn rejects_oversize() {
        let s = "a".repeat(BuildSlug::MAX_LEN + 1);
        let err = BuildSlug::new(s).unwrap_err();
        assert!(matches!(err, DomainError::BuildSlugInvalid { .. }));
    }

    #[test]
    fn accepts_max_length() {
        let s = "a".repeat(BuildSlug::MAX_LEN);
        BuildSlug::new(s).unwrap();
    }
}
