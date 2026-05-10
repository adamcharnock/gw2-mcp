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
    pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
        let trimmed = raw.into().trim().to_owned();
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
}
