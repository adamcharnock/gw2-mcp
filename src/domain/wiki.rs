//! Wiki search domain types.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::error::DomainError;

/// A canonicalised, non-empty search query.
///
/// Holds both the original (for display in responses) and the normalised
/// (lowercased + trimmed) form (for cache key derivation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    original: String,
    normalised: String,
}

impl SearchQuery {
    pub fn new(raw: impl Into<String>) -> Result<Self, DomainError> {
        let original = raw.into().trim().to_owned();
        if original.is_empty() {
            return Err(DomainError::SearchQueryEmpty);
        }
        let normalised = original.to_lowercase();
        Ok(Self {
            original,
            normalised,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.original
    }

    #[must_use]
    pub fn normalised(&self) -> &str {
        &self.normalised
    }
}

impl std::fmt::Display for SearchQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.original)
    }
}

/// A bounded result count for wiki searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchLimit(u32);

impl SearchLimit {
    pub const MIN: u32 = 1;
    pub const MAX: u32 = 50;
    pub const DEFAULT: u32 = 5;

    pub fn new(limit: u32) -> Result<Self, DomainError> {
        if !(Self::MIN..=Self::MAX).contains(&limit) {
            return Err(DomainError::SearchLimitOutOfRange {
                got: limit,
                max: Self::MAX,
            });
        }
        Ok(Self(limit))
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Default for SearchLimit {
    fn default() -> Self {
        // SAFETY: DEFAULT is within MIN..=MAX by construction.
        Self(Self::DEFAULT)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    /// Page extract (intro paragraph). Pre-enriched via a follow-up
    /// `prop=extracts` fetch — the LLM only needs one prose blob per
    /// result, so we surface this and drop the search-API's `snippet`
    /// plus metadata fields (`timestamp`, `page_id`, `size`,
    /// `word_count`) which were token bloat for typical answers.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub extract: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchResponse {
    pub query: String,
    pub results: Vec<SearchResult>,
    pub total: usize,
    pub searched_at: DateTime<Utc>,
    /// Minutes since `searched_at` (~0 on fresh calls; non-zero when
    /// the response came out of cache). Convention pair for the LLM.
    pub searched_minutes_ago: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_query_rejects_empty() {
        assert!(matches!(
            SearchQuery::new("").unwrap_err(),
            DomainError::SearchQueryEmpty
        ));
        assert!(matches!(
            SearchQuery::new("   \t\n").unwrap_err(),
            DomainError::SearchQueryEmpty
        ));
    }

    #[test]
    fn search_query_normalises() {
        let q = SearchQuery::new("  Dragon Bash  ").unwrap();
        assert_eq!(q.as_str(), "Dragon Bash");
        assert_eq!(q.normalised(), "dragon bash");
    }

    #[test]
    fn search_limit_enforces_bounds() {
        assert!(SearchLimit::new(0).is_err());
        assert!(SearchLimit::new(SearchLimit::MAX + 1).is_err());
        assert_eq!(SearchLimit::new(5).unwrap().get(), 5);
        assert_eq!(SearchLimit::default().get(), SearchLimit::DEFAULT);
    }

    #[test]
    fn search_result_round_trip() {
        let r = SearchResult {
            title: "Dragon Bash".to_owned(),
            url: "https://wiki.guildwars2.com/wiki/Dragon%20Bash".to_owned(),
            extract: "Festival".to_owned(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: SearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }
}
