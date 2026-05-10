//! Validation errors raised when constructing domain values.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("api key is empty")]
    ApiKeyEmpty,

    #[error("api key is not a valid Guild Wars 2 API key (got {len} chars; expected at least 64)")]
    ApiKeyMalformed { len: usize },

    #[error("currency id must be positive (got {got})")]
    CurrencyIdInvalid { got: i64 },

    #[error("skill id must be positive (got {got})")]
    SkillIdInvalid { got: i64 },

    #[error("trait id must be positive (got {got})")]
    TraitIdInvalid { got: i64 },

    #[error("specialization id must be positive (got {got})")]
    SpecializationIdInvalid { got: i64 },

    #[error("item id must be positive (got {got})")]
    ItemIdInvalid { got: i64 },

    #[error("character name must be 1..32 chars (got {got_len})")]
    CharacterNameInvalid { got_len: usize },

    #[error("build chat code must start with `[&` and end with `]` (got: {got_prefix})")]
    BuildCodeMalformed { got_prefix: String },

    #[error("search query is empty after trimming")]
    SearchQueryEmpty,

    #[error("search limit out of range (got {got}; allowed 1..={max})")]
    SearchLimitOutOfRange { got: u32, max: u32 },
}
