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
    ApiKey, BuildSlug, CharacterName, Currency, CurrencyId, Item, ItemId, SearchLimit, SearchQuery,
    SearchResult, Skill, SkillId, Specialization, SpecializationId, Trait, TraitId, WalletEntry,
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
    #[error("could not reach the Guild Wars 2 API ({0}) — check your network and try again")]
    Transport(String),

    /// The GW2 API responded but with an error. Carries the most useful
    /// info we can extract: the inner `text` payload (GW2 errors are
    /// shaped `{"text":"..."}`) plus the raw status for diagnostics.
    #[error("Guild Wars 2 API error ({status}): {message}")]
    Upstream { status: u16, message: String },

    #[error(
        "could not understand the Guild Wars 2 API response — this usually means the API \
         changed shape; please open an issue. Detail: {0}"
    )]
    Decode(String),

    #[error(
        "the Guild Wars 2 API rejected this key. Verify the key at \
         https://account.arena.net/applications and check it has the required scopes \
         (`account` + `wallet` for get_wallet; `account` + `characters` + `builds` for \
         get_character_build)."
    )]
    Unauthorized,

    /// 403 with body matching `requires scope`. The key is otherwise
    /// valid but lacks the named scope. We carry the scope name so the
    /// LLM can tell the user exactly which checkbox to tick.
    #[error(
        "the Guild Wars 2 API rejected this key for missing scope: {needed}. Generate a new key \
         at https://account.arena.net/applications and ensure that scope is checked."
    )]
    MissingScope { needed: String },

    #[error("character `{name}` does not exist on this account")]
    CharacterNotFound { name: String },

    /// 429 from the GW2 API. Carries the parsed `Retry-After` value if
    /// the upstream supplied one — surfaced to the user so they don't
    /// retry too eagerly.
    #[error("{}", rate_limited_message(*.0))]
    RateLimited(Option<std::time::Duration>),
}

/// Render the rate-limit message with a concrete retry hint when we
/// have one. Defined here (not as an `impl Display` body) so the
/// `#[error]` attribute above stays a single-line literal.
fn rate_limited_message(retry_after: Option<std::time::Duration>) -> String {
    match retry_after {
        Some(d) if d.as_secs() > 0 => format!(
            "the Guild Wars 2 API rate limit was hit. Try again in {} seconds.",
            d.as_secs()
        ),
        _ => "the Guild Wars 2 API rate limit was hit. Try again in a few seconds.".into(),
    }
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

    /// `/v2/skills?ids=…`
    async fn fetch_skills(&self, ids: &[SkillId]) -> Result<BTreeMap<SkillId, Skill>, Gw2ApiError>;

    /// `/v2/traits?ids=…`
    async fn fetch_traits(&self, ids: &[TraitId]) -> Result<BTreeMap<TraitId, Trait>, Gw2ApiError>;

    /// `/v2/specializations?ids=…`
    async fn fetch_specializations(
        &self,
        ids: &[SpecializationId],
    ) -> Result<BTreeMap<SpecializationId, Specialization>, Gw2ApiError>;

    /// `/v2/items?ids=…`
    async fn fetch_items(&self, ids: &[ItemId]) -> Result<BTreeMap<ItemId, Item>, Gw2ApiError>;

    /// `/v2/characters/:name/buildtabs?tabs=all` — requires `builds` scope.
    /// Returned as raw JSON values (variants too rich to be worth typing).
    async fn fetch_buildtabs(
        &self,
        key: &ApiKey,
        name: &CharacterName,
    ) -> Result<Vec<serde_json::Value>, Gw2ApiError>;

    /// `/v2/characters/:name/equipmenttabs?tabs=all` — requires `builds` scope.
    async fn fetch_equipmenttabs(
        &self,
        key: &ApiKey,
        name: &CharacterName,
    ) -> Result<Vec<serde_json::Value>, Gw2ApiError>;
}

// ---------------------------------------------------------------------------
// Curated build catalogs (Phase B)
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("no such build source: {0}")]
    NoSuchSource(String),

    // `source` is reserved by thiserror; use `source_name` everywhere.
    #[error("build not found in {source_name}: {slug}")]
    NotFound { source_name: String, slug: String },

    #[error("transport error from {source_name}: {message}")]
    Transport {
        source_name: String,
        message: String,
    },

    #[error("parse error from {source_name}: {message}")]
    Parse {
        source_name: String,
        message: String,
    },
}

/// What to filter the catalog listing by. None means "no filter on that
/// dimension". Sources interpret unknown values by returning an empty
/// listing rather than erroring.
#[derive(Debug, Clone, Default)]
pub struct CatalogFilter {
    pub profession: Option<String>,
    pub gamemode: Option<String>,
    pub limit: Option<u32>,
}

/// Lightweight summary returned from a catalog listing — enough for an
/// LLM to pick which build to fetch in detail.
#[derive(
    Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, schemars::JsonSchema,
)]
pub struct BuildSummary {
    pub slug: String,
    pub title: String,
    pub profession: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elite_spec: Option<String>,
    pub role: String,
    pub gamemode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<String>,
    pub source: String,
    pub source_url: String,
}

/// Detailed build view. We keep this loose (`details: serde_json::Value`)
/// because each catalog's data is shaped differently and the LLM is the
/// consumer — typing every variant would be premature.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, schemars::JsonSchema)]
pub struct BuildDetail {
    pub summary: BuildSummary,
    pub details: serde_json::Value,
    /// Plain-text description / rotation notes if the source provides them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Build chat code, if the source publishes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_code: Option<String>,
}

#[async_trait]
pub trait BuildCatalog: Send + Sync + 'static {
    /// Stable identifier — used as the `source` field in MCP tool calls.
    fn name(&self) -> &'static str;

    async fn list(&self, filter: &CatalogFilter) -> Result<Vec<BuildSummary>, CatalogError>;

    /// Fetch a single build by its (already-validated) slug. The
    /// [`BuildSlug`] newtype guarantees the value is safe to interpolate
    /// into a URL path (no `..`, no newlines, ≤ 256 chars, all-lowercase).
    async fn fetch(&self, slug: &BuildSlug) -> Result<BuildDetail, CatalogError>;
}

/// Tiny registry so `Service` can route by source name without owning a
/// fixed set of adapters.
pub struct CatalogRegistry {
    catalogs: std::collections::BTreeMap<&'static str, std::sync::Arc<dyn BuildCatalog>>,
}

impl CatalogRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            catalogs: std::collections::BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with(mut self, c: std::sync::Arc<dyn BuildCatalog>) -> Self {
        self.catalogs.insert(c.name(), c);
        self
    }

    pub fn get(&self, name: &str) -> Option<&std::sync::Arc<dyn BuildCatalog>> {
        self.catalogs.get(name)
    }

    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.catalogs.keys().copied().collect()
    }
}

impl Default for CatalogRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Build code decoder
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum BuildCodeError {
    #[error("malformed chat code: {0}")]
    Malformed(String),
}

/// Decodes GW2 build template chat codes (`[&Dw...=]`) into structured
/// data. Behind a port so we can swap the underlying decoder later
/// (currently `chatr`).
pub trait BuildCodeDecoder: Send + Sync + 'static {
    fn decode(
        &self,
        code: &crate::domain::BuildChatCode,
    ) -> Result<serde_json::Value, BuildCodeError>;
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
