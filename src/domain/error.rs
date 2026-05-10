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

    #[error("search query is empty after trimming")]
    SearchQueryEmpty,

    #[error("search limit out of range (got {got}; allowed 1..={max})")]
    SearchLimitOutOfRange { got: u32, max: u32 },
}
