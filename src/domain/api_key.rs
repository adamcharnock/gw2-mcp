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
    pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
        let trimmed = raw.into().trim().to_owned();
        if trimmed.is_empty() {
            return Err(DomainError::ApiKeyEmpty);
        }
        if trimmed.len() < Self::MIN_LEN {
            return Err(DomainError::ApiKeyMalformed { len: trimmed.len() });
        }
        Ok(Self(trimmed))
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
    fn display_redacts_secret() {
        let key = ApiKey::new(valid_key()).unwrap();
        let formatted = format!("{key}");
        assert!(!formatted.contains(&valid_key()));
        assert!(formatted.contains("redacted"));
    }
}
