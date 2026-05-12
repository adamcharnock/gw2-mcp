//! Shared substring-matching helper for caller-side filters.
//!
//! Used by storage-tab tools (`get_account_materials`,
//! `get_account_bank`, `get_character_inventory`) to apply
//! `name_contains` filters. Kept deliberately simple — case-folded,
//! token-based "all needle tokens appear in haystack" — so all the
//! tools behave the same way and callers don't have to guess at each
//! tool's matching rules.
//!
//! Distinct from the full-text search index in `service/search.rs`
//! (which is BM25-ranked across a Tantivy corpus); this is a tiny
//! in-memory predicate that runs per-row over an already-fetched
//! response.

/// Returns true when `haystack` contains every whitespace-delimited
/// token from `needle`, case-insensitively.
///
/// An empty (or whitespace-only) needle matches anything — callers
/// are expected to skip the filter entirely when the arg wasn't
/// provided rather than passing `""`, but this graceful fallback
/// keeps the call sites simple.
///
/// Tokens are independent and order-agnostic: `"mystic coin"` matches
/// `"Coin, Mystic"` and `"Mystic Coin"` both. Each token is a
/// substring check, not a word-boundary check — `"shard"` matches
/// `"Obsidian Shards"`.
#[must_use]
pub fn name_contains_match(needle: &str, haystack: &str) -> bool {
    let needle = needle.trim();
    if needle.is_empty() {
        return true;
    }
    let haystack_lower = haystack.to_lowercase();
    needle
        .split_whitespace()
        .all(|tok| haystack_lower.contains(&tok.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::name_contains_match;

    #[test]
    fn exact_substring_matches() {
        assert!(name_contains_match("ecto", "Glob of Ectoplasm"));
        assert!(name_contains_match("mystic", "Mystic Coin"));
    }

    #[test]
    fn case_insensitive() {
        assert!(name_contains_match("ECTO", "Glob of Ectoplasm"));
        assert!(name_contains_match("Glob", "glob of ectoplasm"));
    }

    #[test]
    fn multi_token_any_order() {
        assert!(name_contains_match("mystic coin", "Coin, Mystic"));
        assert!(name_contains_match("coin mystic", "Mystic Coin"));
        assert!(name_contains_match(
            "bloodstone dust",
            "Pile of Bloodstone Dust"
        ));
    }

    #[test]
    fn partial_word_match_allowed() {
        // "shard" hits "Shards" (substring, not word-boundary).
        assert!(name_contains_match("shard", "Obsidian Shards"));
    }

    #[test]
    fn no_match_returns_false() {
        assert!(!name_contains_match("ectoplasm", "Mystic Coin"));
        assert!(!name_contains_match("mystic dust", "Mystic Coin"));
    }

    #[test]
    fn empty_needle_matches_anything() {
        assert!(name_contains_match("", "literally anything"));
        assert!(name_contains_match("   ", "Mystic Coin"));
    }
}
