//! GW2 character name newtype.

use super::error::DomainError;

/// A non-empty, length-bounded character name.
///
/// GW2 character names are 3-19 chars in-game; we accept 1-32 to be
/// permissive without exposing arbitrary-length strings to the URL builder.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CharacterName(String);

impl CharacterName {
    const MAX_LEN: usize = 32;

    pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
        let trimmed = raw.into().trim().to_owned();
        if trimmed.is_empty() || trimmed.len() > Self::MAX_LEN {
            return Err(DomainError::CharacterNameInvalid {
                got_len: trimmed.len(),
            });
        }
        Ok(Self(trimmed))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CharacterName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        assert!(CharacterName::new("").is_err());
        assert!(CharacterName::new("   ").is_err());
    }

    #[test]
    fn rejects_too_long() {
        let too_long = "a".repeat(33);
        assert!(CharacterName::new(too_long).is_err());
    }

    #[test]
    fn accepts_typical_name() {
        let n = CharacterName::new("Tarnished Coast").unwrap();
        assert_eq!(n.as_str(), "Tarnished Coast");
    }

    #[test]
    fn trims_whitespace() {
        let n = CharacterName::new("  Hero  ").unwrap();
        assert_eq!(n.as_str(), "Hero");
    }
}
