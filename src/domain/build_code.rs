//! Build template chat code newtype.
//!
//! GW2 build chat codes look like `[&Dw...=]` — bracketed, base64-ish, and
//! always start with magic byte `0x0D` after decoding. We validate the
//! shape only; payload decoding lives in the build-code adapter so we
//! don't pull binary-parser deps into the domain crate.

use super::error::DomainError;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BuildChatCode(String);

impl BuildChatCode {
    /// Hard upper bound on accepted chat-code length.
    ///
    /// Real GW2 codes are ~80–100 chars. We cap input at 512 so a
    /// hostile / malformed caller can't hand us a multi-megabyte blob —
    /// `chatr::ChatCode::build` would otherwise base64-decode the whole
    /// thing on the request thread.
    pub const MAX_LEN: usize = 512;

    pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
        let trimmed = raw.into().trim().to_owned();
        if trimmed.len() > Self::MAX_LEN {
            // Don't echo the whole oversized input; preview is plenty.
            let preview: String = trimmed.chars().take(16).collect();
            return Err(DomainError::BuildCodeTooLong {
                len: trimmed.len(),
                max: Self::MAX_LEN,
                got_prefix: preview,
            });
        }
        if trimmed.starts_with("[&") && trimmed.ends_with(']') && trimmed.len() > 4 {
            Ok(Self(trimmed))
        } else {
            // Show only the leading 16 chars so we don't echo a long invalid blob.
            let preview: String = trimmed.chars().take(16).collect();
            Err(DomainError::BuildCodeMalformed {
                got_prefix: preview,
            })
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BuildChatCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unbracketed() {
        assert!(BuildChatCode::new("Dwabc=").is_err());
        assert!(BuildChatCode::new("").is_err());
    }

    #[test]
    fn accepts_bracketed_code() {
        let c =
            BuildChatCode::new("[&DQYpGyU+OD90AAAAywAAAI8AAACRAAAAJgAAAAAAAAAAAAAAAAAAAAAAAAA=]")
                .unwrap();
        assert!(c.as_str().starts_with("[&"));
    }

    #[test]
    fn accepts_input_at_max_length() {
        // Build a 512-char string that still parses as bracketed.
        let mut s = String::with_capacity(BuildChatCode::MAX_LEN);
        s.push_str("[&");
        s.push_str(&"A".repeat(BuildChatCode::MAX_LEN - 3));
        s.push(']');
        assert_eq!(s.len(), BuildChatCode::MAX_LEN);
        BuildChatCode::new(&s).expect("MAX_LEN must be accepted");
    }

    #[test]
    fn rejects_input_above_max_length() {
        let mut s = String::with_capacity(BuildChatCode::MAX_LEN + 1);
        s.push_str("[&");
        s.push_str(&"A".repeat(BuildChatCode::MAX_LEN - 2));
        s.push(']');
        assert_eq!(s.len(), BuildChatCode::MAX_LEN + 1);
        let err = BuildChatCode::new(&s).unwrap_err();
        assert!(
            matches!(err, DomainError::BuildCodeTooLong { .. }),
            "expected BuildCodeTooLong, got {err:?}"
        );
    }

    #[test]
    fn rejects_garbage_input() {
        let err = BuildChatCode::new("not a chat code at all").unwrap_err();
        assert!(matches!(err, DomainError::BuildCodeMalformed { .. }));
    }

    #[test]
    fn rejects_huge_blob_before_format_check() {
        // Even an unbracketed multi-megabyte blob hits the size cap, not the
        // format check — proves we don't allocate work proportional to input.
        let huge = "x".repeat(10_000);
        let err = BuildChatCode::new(huge).unwrap_err();
        assert!(matches!(err, DomainError::BuildCodeTooLong { .. }));
    }
}
