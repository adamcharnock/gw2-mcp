//! Trait definitions for everything the [service](crate::service) depends on.
//!
//! Every external concern enters the codebase through one of these traits.
//! Domain code never imports `adapters/` directly — only `ports`. This means
//! the service can be tested with mocks (or in-memory fakes) without ever
//! making a network call.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::domain::{
    ApiKey, Currency, CurrencyId, SearchLimit, SearchQuery, SearchResult, WalletEntry,
};

// ---------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------

/// Source of the current time.
///
/// Inject a `MockClock` in tests to make any time-dependent behaviour
/// deterministic. The trait is `Send + Sync` so it can live behind an `Arc`
/// across async tasks.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Utc>;
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("failed to serialise cache value: {0}")]
    Serialise(#[source] serde_json::Error),

    #[error("failed to deserialise cache value: {0}")]
    Deserialise(#[source] serde_json::Error),
}

/// A simple TTL cache port.
///
/// Values are stored as JSON strings — keeping the interface object-safe
/// and avoiding generic gymnastics. Adapters are free to use any in-memory
/// representation internally.
#[async_trait]
pub trait Cache: Send + Sync + 'static {
    /// Insert `value` under `key` with the given TTL.
    async fn set(&self, key: &str, value: String, ttl: Duration);

    /// Fetch the value at `key`, if present and not expired.
    async fn get(&self, key: &str) -> Option<String>;

    /// Remove a key. No-op if absent.
    async fn delete(&self, key: &str);

    /// Number of entries currently cached. Mostly useful for tests.
    async fn len(&self) -> usize;

    /// Whether the cache is empty. Mostly useful for tests.
    async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

// ---------------------------------------------------------------------------
// GW2 API
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum Gw2ApiError {
    #[error("transport error: {0}")]
    Transport(String),

    #[error("GW2 API returned status {status}: {body}")]
    Status { status: u16, body: String },

    #[error("failed to decode GW2 response: {0}")]
    Decode(String),

    #[error("invalid API key (rejected by GW2 API)")]
    Unauthorized,
}

/// Read-only Guild Wars 2 API client.
#[async_trait]
pub trait Gw2Api: Send + Sync + 'static {
    /// `/v2/account/wallet` — requires an API key with `wallet` scope.
    async fn fetch_wallet(&self, key: &ApiKey) -> Result<Vec<WalletEntry>, Gw2ApiError>;

    /// `/v2/currencies` (no parameters) — returns every known currency id.
    async fn fetch_currency_ids(&self) -> Result<Vec<CurrencyId>, Gw2ApiError>;

    /// `/v2/currencies?ids=…` — fetch metadata for specific ids.
    async fn fetch_currencies(
        &self,
        ids: &[CurrencyId],
    ) -> Result<BTreeMap<CurrencyId, Currency>, Gw2ApiError>;
}

// ---------------------------------------------------------------------------
// Wiki
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum WikiError {
    #[error("transport error: {0}")]
    Transport(String),

    #[error("wiki API returned status {status}: {body}")]
    Status { status: u16, body: String },

    #[error("failed to decode wiki response: {0}")]
    Decode(String),
}

/// Read-only Guild Wars 2 wiki client.
#[async_trait]
pub trait Wiki: Send + Sync + 'static {
    async fn search(
        &self,
        query: &SearchQuery,
        limit: SearchLimit,
    ) -> Result<Vec<SearchResult>, WikiError>;

    /// Returns the leading prose extract for a page. Empty string if missing.
    async fn fetch_extract(&self, title: &str) -> Result<String, WikiError>;
}
