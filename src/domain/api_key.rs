//! Guild Wars 2 API key newtype.
//!
//! Wraps a raw key so it can't be confused with any other String, and
//! prevents accidental logging of the secret.

use sha2::{Digest, Sha256};

use super::error::DomainError;

/// A validated Guild Wars 2 API key.
///
/// `Display` and `Debug` implementations redact the secret. Use
/// [`ApiKey::expose`] only when sending the key to the GW2 API.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// Minimum sensible length for a GW2 API key (real keys are ~72 chars).
    /// We accept anything ≥ 64 to be lenient with future format changes.
    const MIN_LEN: usize = 64;

    /// Construct a new API key after trimming and validating.
    ///
    /// Tolerates a leading case-insensitive `bearer ` prefix because users
    /// commonly paste the value straight out of an HTTP `Authorization`
    /// header. The prefix is stripped before length validation, so the
    /// raw secret is what gets persisted, fingerprinted, and sent upstream.
    pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
        let trimmed = raw.into().trim().to_owned();
        if trimmed.is_empty() {
            return Err(DomainError::ApiKeyEmpty);
        }
        // Strip "Bearer "/"BEARER "/"bearer " (any case) + any trailing
        // whitespace before validation, then re-check empty.
        let stripped = strip_bearer_prefix(&trimmed);
        if stripped.is_empty() {
            return Err(DomainError::ApiKeyEmpty);
        }
        if stripped.len() < Self::MIN_LEN {
            return Err(DomainError::ApiKeyMalformed {
                len: stripped.len(),
            });
        }
        Ok(Self(stripped.to_owned()))
    }

    /// Return the underlying secret. Use only when calling the GW2 API.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Short fingerprint suitable for cache keys and structured log fields.
    /// Truncated SHA-256 — not reversible, safe to persist.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let hash = Sha256::digest(self.0.as_bytes());
        // First 8 bytes (16 hex chars) is plenty for cache-key uniqueness.
        hex::encode(&hash[..8])
    }
}

/// Strip a leading `bearer` prefix (any case) and any whitespace that
/// follows it. Returns the input unchanged if no prefix matches.
fn strip_bearer_prefix(s: &str) -> &str {
    let lower_prefix = "bearer";
    if s.len() < lower_prefix.len() {
        return s;
    }
    let head = &s[..lower_prefix.len()];
    if !head.eq_ignore_ascii_case(lower_prefix) {
        return s;
    }
    let tail = &s[lower_prefix.len()..];
    // Require at least one whitespace char between the prefix and the
    // secret — refuses to eat the first 6 chars of a key whose payload
    // happens to start with `bearer<...>`.
    if !tail.chars().next().is_some_and(char::is_whitespace) {
        return s;
    }
    tail.trim_start()
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ApiKey")
            .field(&format_args!("<redacted:{}>", self.fingerprint()))
            .finish()
    }
}

impl std::fmt::Display for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted:{}>", self.fingerprint())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_key() -> String {
        // 72-char key shaped like a real GW2 key.
        "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE-FFFFFFFF-GGGG-HHHH-IIII-JJJJJJJJJJJJ".to_owned()
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(ApiKey::new("").unwrap_err(), DomainError::ApiKeyEmpty);
        assert_eq!(ApiKey::new("   ").unwrap_err(), DomainError::ApiKeyEmpty);
    }

    #[test]
    fn rejects_too_short() {
        let err = ApiKey::new("short").unwrap_err();
        assert!(matches!(err, DomainError::ApiKeyMalformed { .. }));
    }

    #[test]
    fn accepts_well_formed_key() {
        let key = ApiKey::new(valid_key()).unwrap();
        assert_eq!(key.expose(), valid_key());
    }

    #[test]
    fn trims_whitespace() {
        let key = ApiKey::new(format!("  {}  ", valid_key())).unwrap();
        assert_eq!(key.expose(), valid_key());
    }

    #[test]
    fn fingerprint_is_stable_and_short() {
        let key = ApiKey::new(valid_key()).unwrap();
        let fp = key.fingerprint();
        assert_eq!(fp.len(), 16); // 8 bytes hex-encoded
        assert_eq!(fp, key.fingerprint(), "fingerprint must be deterministic");
    }

    #[test]
    fn debug_redacts_secret() {
        let key = ApiKey::new(valid_key()).unwrap();
        let formatted = format!("{key:?}");
        assert!(
            !formatted.contains(&valid_key()),
            "raw key leaked: {formatted}"
        );
        assert!(formatted.contains("redacted"));
    }

    #[test]
    fn strips_bearer_prefix_lower() {
        let key = ApiKey::new(format!("bearer {}", valid_key())).unwrap();
        assert_eq!(key.expose(), valid_key());
    }

    #[test]
    fn strips_bearer_prefix_pascal() {
        let key = ApiKey::new(format!("Bearer {}", valid_key())).unwrap();
        assert_eq!(key.expose(), valid_key());
    }

    #[test]
    fn strips_bearer_prefix_upper_with_multiple_spaces() {
        let key = ApiKey::new(format!("BEARER  \t  {}", valid_key())).unwrap();
        assert_eq!(key.expose(), valid_key());
    }

    #[test]
    fn strips_bearer_prefix_after_outer_whitespace() {
        // Whitespace before AND after the prefix should both be stripped.
        let key = ApiKey::new(format!("  Bearer  {}  ", valid_key())).unwrap();
        assert_eq!(key.expose(), valid_key());
    }

    #[test]
    fn no_bearer_prefix_unchanged() {
        let key = ApiKey::new(valid_key()).unwrap();
        assert_eq!(key.expose(), valid_key());
    }

    #[test]
    fn does_not_strip_bearer_substring_inside_key() {
        // A key that happens to start with the letters "bearer" without a
        // separator must NOT have those bytes eaten.
        let raw = format!("bearer{}", valid_key());
        let key = ApiKey::new(&raw).unwrap();
        assert_eq!(key.expose(), raw);
    }

    #[test]
    fn rejects_bearer_with_no_token() {
        // Just the prefix + whitespace + nothing else. Outer trim turns
        // this into "Bearer" with no trailing space, so the bearer-prefix
        // strip is a no-op and we end up with a too-short key. We just
        // need to confirm it errors — either ApiKeyEmpty or
        // ApiKeyMalformed is acceptable.
        assert!(ApiKey::new("Bearer ").is_err());
        assert!(ApiKey::new("Bearer  \t  ").is_err());
    }

    #[test]
    fn display_redacts_secret() {
        let key = ApiKey::new(valid_key()).unwrap();
        let formatted = format!("{key}");
        assert!(!formatted.contains(&valid_key()));
        assert!(formatted.contains("redacted"));
    }
}
