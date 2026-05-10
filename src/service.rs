//! Business logic. Orchestrates ports without ever knowing which concrete
//! adapter is plugged in. Caching policy lives here — adapters are dumb.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tracing::{debug, warn};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::BuildChatCode;
use crate::domain::{
    ApiKey, CharacterName, Currency, CurrencyId, SearchLimit, SearchQuery, SearchResponse, Skill,
    SkillId, Specialization, SpecializationId, Trait, TraitId, WalletEntry, WalletInfo,
};
use crate::ports::{
    BuildCodeDecoder, BuildCodeError, Cache, CacheError, Clock, Gw2Api, Gw2ApiError, Wiki,
    WikiError,
};

// Cache TTLs centralised so changes are atomic.
pub const STATIC_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 365); // 1 year
pub const WIKI_TTL: Duration = Duration::from_secs(60 * 60 * 24); // 1 day
pub const WALLET_TTL: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// Service errors are pass-through wrappers — the inner port errors are
/// already user-facing (see `Gw2ApiError`, `WikiError`, etc.). Adding a
/// "upstream X error: " prefix would just push the useful sentence further
/// down the user's eye-line.
#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("{0}")]
    Gw2(#[from] Gw2ApiError),

    #[error("{0}")]
    Wiki(#[from] WikiError),

    #[error("{0}")]
    Cache(#[from] CacheError),

    #[error("{0}")]
    BuildCode(#[from] BuildCodeError),

    #[error("{0}")]
    Catalog(#[from] crate::ports::CatalogError),
}

/// Orchestrates GW2 / wiki access with a TTL cache in front.
///
/// Cloning is cheap — internals are `Arc`-wrapped trait objects.
#[derive(Clone)]
pub struct Service {
    gw2: Arc<dyn Gw2Api>,
    wiki: Arc<dyn Wiki>,
    cache: Arc<dyn Cache>,
    clock: Arc<dyn Clock>,
    build_decoder: Arc<dyn BuildCodeDecoder>,
    catalogs: Arc<crate::ports::CatalogRegistry>,
}

impl Service {
    pub fn new(
        gw2: Arc<dyn Gw2Api>,
        wiki: Arc<dyn Wiki>,
        cache: Arc<dyn Cache>,
        clock: Arc<dyn Clock>,
        build_decoder: Arc<dyn BuildCodeDecoder>,
        catalogs: Arc<crate::ports::CatalogRegistry>,
    ) -> Self {
        Self {
            gw2,
            wiki,
            cache,
            clock,
            build_decoder,
            catalogs,
        }
    }

    /// Decode a `[&Dw…]` build chat code into structured JSON.
    pub fn decode_build_code(
        &self,
        code: &BuildChatCode,
    ) -> Result<serde_json::Value, ServiceError> {
        Ok(self.build_decoder.decode(code)?)
    }

    /// List the names of registered curated-build sources.
    #[must_use]
    pub fn list_catalogs(&self) -> Vec<&'static str> {
        self.catalogs.names()
    }

    /// List builds from a named catalog. Cached for `WIKI_TTL` (24 h) per
    /// (source, filter) — listings are large and rarely change within a
    /// day. The filter is fingerprinted into the cache key so different
    /// filters share storage but don't collide.
    pub async fn list_catalog_builds(
        &self,
        source: &str,
        filter: crate::ports::CatalogFilter,
    ) -> Result<Vec<crate::ports::BuildSummary>, ServiceError> {
        let cat = self
            .catalogs
            .get(source)
            .ok_or_else(|| crate::ports::CatalogError::NoSuchSource(source.to_owned()))?;

        let cache_key = catalog_list_cache_key(source, &filter);
        if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(cached) = serde_json::from_str::<Vec<crate::ports::BuildSummary>>(&json)
        {
            debug!(source, "catalog list cache hit");
            return Ok(cached);
        }

        let summaries = cat.list(&filter).await?;
        if let Ok(json) = serde_json::to_string(&summaries) {
            self.cache.set(&cache_key, json, WIKI_TTL).await;
        }
        Ok(summaries)
    }

    /// Fetch a specific build from a catalog. Cached for `WIKI_TTL` per
    /// (source, slug). 1-day TTL is enough — catalogs publish updates on
    /// patch days and we don't need read-after-write consistency.
    pub async fn get_catalog_build(
        &self,
        source: &str,
        slug: &str,
    ) -> Result<crate::ports::BuildDetail, ServiceError> {
        let cat = self
            .catalogs
            .get(source)
            .ok_or_else(|| crate::ports::CatalogError::NoSuchSource(source.to_owned()))?;

        let cache_key = catalog_fetch_cache_key(source, slug);
        if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(cached) = serde_json::from_str::<crate::ports::BuildDetail>(&json)
        {
            debug!(source, slug, "catalog fetch cache hit");
            return Ok(cached);
        }

        let detail = cat.fetch(slug).await?;
        if let Ok(json) = serde_json::to_string(&detail) {
            self.cache.set(&cache_key, json, WIKI_TTL).await;
        }
        Ok(detail)
    }

    // -----------------------------------------------------------------
    // Wallet
    // -----------------------------------------------------------------

    /// Fetch a wallet for the given API key.
    ///
    /// Cache key is derived from the API key fingerprint so two LLM clients
    /// using the same key share the cache without ever exposing the secret.
    pub async fn get_wallet(&self, key: &ApiKey) -> Result<WalletInfo, ServiceError> {
        let cache_key = wallet_cache_key(key);

        if let Some(json) = self.cache.get(&cache_key).await {
            match serde_json::from_str::<WalletInfo>(&json) {
                Ok(info) => {
                    debug!(fingerprint = %key.fingerprint(), "wallet cache hit");
                    return Ok(info);
                }
                // A poisoned cache entry should never block the request — log
                // and fall through to fetch fresh.
                Err(e) => warn!(error = ?e, "wallet cache poisoned; refetching"),
            }
        }

        debug!(fingerprint = %key.fingerprint(), "wallet cache miss");

        let entries = self.gw2.fetch_wallet(key).await?;

        // Best-effort metadata enrichment — empty map is acceptable.
        let currencies = match self.fetch_currencies_for(&entries).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = ?e, "failed to enrich wallet with currency metadata");
                BTreeMap::new()
            }
        };

        let info = WalletInfo {
            total_currencies: entries.len(),
            entries,
            currencies,
            updated_at: self.clock.now(),
        };

        // Cache failures aren't fatal — they just mean the next call refetches.
        match serde_json::to_string(&info) {
            Ok(json) => self.cache.set(&cache_key, json, WALLET_TTL).await,
            Err(e) => warn!(error = ?e, "failed to serialise wallet for cache"),
        }

        Ok(info)
    }

    // -----------------------------------------------------------------
    // Currencies
    // -----------------------------------------------------------------

    /// Fetch metadata for `ids`. Empty `ids` returns every known currency.
    pub async fn get_currencies(
        &self,
        ids: &[CurrencyId],
    ) -> Result<BTreeMap<CurrencyId, Currency>, ServiceError> {
        if ids.is_empty() {
            return self.get_all_currencies().await;
        }

        let mut result = BTreeMap::new();
        let mut missing = Vec::new();

        for id in ids {
            let key = currency_cache_key(*id);
            match self.cache.get(&key).await {
                Some(json) => match serde_json::from_str::<Currency>(&json) {
                    Ok(c) => {
                        result.insert(*id, c);
                    }
                    Err(e) => {
                        warn!(currency_id = %id, error = ?e, "currency cache poisoned");
                        missing.push(*id);
                    }
                },
                None => missing.push(*id),
            }
        }

        if !missing.is_empty() {
            let fetched = self.gw2.fetch_currencies(&missing).await?;
            for (id, currency) in fetched {
                if let Ok(json) = serde_json::to_string(&currency) {
                    self.cache
                        .set(&currency_cache_key(id), json, STATIC_TTL)
                        .await;
                }
                result.insert(id, currency);
            }
        }

        Ok(result)
    }

    async fn get_all_currencies(&self) -> Result<BTreeMap<CurrencyId, Currency>, ServiceError> {
        const KEY: &str = "currencies:list";

        if let Some(json) = self.cache.get(KEY).await
            && let Ok(map) = serde_json::from_str::<BTreeMap<CurrencyId, Currency>>(&json)
        {
            return Ok(map);
        }

        let ids = self.gw2.fetch_currency_ids().await?;
        let map = self.gw2.fetch_currencies(&ids).await?;

        if let Ok(json) = serde_json::to_string(&map) {
            self.cache.set(KEY, json, STATIC_TTL).await;
        }

        Ok(map)
    }

    async fn fetch_currencies_for(
        &self,
        entries: &[WalletEntry],
    ) -> Result<BTreeMap<CurrencyId, Currency>, ServiceError> {
        let ids: Vec<CurrencyId> = entries.iter().map(|e| e.id).collect();
        self.get_currencies(&ids).await
    }

    // -----------------------------------------------------------------
    // Reference data: skills, traits, specializations
    //
    // Same caching pattern as currencies: per-id JSON cache entries with
    // STATIC_TTL. Empty `ids` is rejected (skills alone are 3000+ entries
    // — fetching the lot is never the right call).
    // -----------------------------------------------------------------

    pub async fn get_skills(
        &self,
        ids: &[SkillId],
    ) -> Result<BTreeMap<SkillId, Skill>, ServiceError> {
        cached_by_id(
            self.cache.as_ref(),
            ids,
            |id| format!("skill:{id}"),
            |missing| async move { self.gw2.fetch_skills(&missing).await.map_err(Into::into) },
        )
        .await
    }

    pub async fn get_traits(
        &self,
        ids: &[TraitId],
    ) -> Result<BTreeMap<TraitId, Trait>, ServiceError> {
        cached_by_id(
            self.cache.as_ref(),
            ids,
            |id| format!("trait:{id}"),
            |missing| async move { self.gw2.fetch_traits(&missing).await.map_err(Into::into) },
        )
        .await
    }

    pub async fn get_specializations(
        &self,
        ids: &[SpecializationId],
    ) -> Result<BTreeMap<SpecializationId, Specialization>, ServiceError> {
        cached_by_id(
            self.cache.as_ref(),
            ids,
            |id| format!("specialization:{id}"),
            |missing| async move {
                self.gw2
                    .fetch_specializations(&missing)
                    .await
                    .map_err(Into::into)
            },
        )
        .await
    }

    // -----------------------------------------------------------------
    // Character build (A.2)
    // -----------------------------------------------------------------

    /// Fetch every build tab and equipment tab for a character.
    ///
    /// Output is cached per (api-key-fingerprint, character) at `WALLET_TTL` —
    /// the data changes whenever the player saves a tab in-game, so the
    /// short TTL avoids stale build advice without spamming the API.
    pub async fn get_character_build(
        &self,
        key: &ApiKey,
        name: &CharacterName,
    ) -> Result<CharacterBuildSnapshot, ServiceError> {
        let cache_key = format!("character_build:{}:{}", key.fingerprint(), name.as_str());

        if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(snap) = serde_json::from_str::<CharacterBuildSnapshot>(&json)
        {
            debug!(character = %name, "character build cache hit");
            return Ok(snap);
        }

        // Fetch in parallel — independent endpoints, no point sequential.
        let (build_tabs, equipment_tabs) = tokio::try_join!(
            self.gw2.fetch_buildtabs(key, name),
            self.gw2.fetch_equipmenttabs(key, name),
        )?;

        let snap = CharacterBuildSnapshot {
            character_name: name.as_str().to_owned(),
            build_tabs,
            equipment_tabs,
            fetched_at: self.clock.now(),
        };

        if let Ok(json) = serde_json::to_string(&snap) {
            self.cache.set(&cache_key, json, WALLET_TTL).await;
        }
        Ok(snap)
    }

    // -----------------------------------------------------------------
    // Wiki
    // -----------------------------------------------------------------

    /// Search the wiki, enriching each hit with a short prose extract.
    pub async fn search_wiki(
        &self,
        query: &SearchQuery,
        limit: SearchLimit,
    ) -> Result<SearchResponse, ServiceError> {
        let cache_key = wiki_search_cache_key(query, limit);

        if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(resp) = serde_json::from_str::<SearchResponse>(&json)
        {
            debug!(query = %query, "wiki search cache hit");
            return Ok(resp);
        }

        let mut results = self.wiki.search(query, limit).await?;
        // Enrich with extracts. A failure on one page must not poison the rest.
        for r in &mut results {
            match self.fetch_or_cache_extract(&r.title).await {
                Ok(extract) => r.extract = extract,
                Err(e) => warn!(title = %r.title, error = ?e, "failed to fetch extract"),
            }
            r.url = wiki_page_url(&r.title);
        }

        let total = results.len();
        let response = SearchResponse {
            query: query.as_str().to_owned(),
            results,
            total,
            searched_at: self.clock.now(),
        };

        if let Ok(json) = serde_json::to_string(&response) {
            self.cache.set(&cache_key, json, WIKI_TTL).await;
        }

        Ok(response)
    }

    async fn fetch_or_cache_extract(&self, title: &str) -> Result<String, ServiceError> {
        let key = wiki_extract_cache_key(title);
        if let Some(extract) = self.cache.get(&key).await {
            return Ok(extract);
        }
        let extract = self.wiki.fetch_extract(title).await?;
        self.cache.set(&key, extract.clone(), WIKI_TTL).await;
        Ok(extract)
    }
}

// ---------------------------------------------------------------------------
// Cache key helpers — module-local so tests can pin them
// ---------------------------------------------------------------------------

fn wallet_cache_key(key: &ApiKey) -> String {
    format!("wallet:{}", key.fingerprint())
}

fn currency_cache_key(id: CurrencyId) -> String {
    format!("currency:detail:{id}")
}

fn wiki_search_cache_key(query: &SearchQuery, limit: SearchLimit) -> String {
    format!("wiki:search:{}:{}", query.normalised(), limit.get())
}

fn wiki_extract_cache_key(title: &str) -> String {
    format!("wiki:extract:{title}")
}

fn catalog_list_cache_key(source: &str, f: &crate::ports::CatalogFilter) -> String {
    // Compact, deterministic — `*` for "no filter on that dimension" so the
    // key is human-readable in logs.
    let prof = f.profession.as_deref().unwrap_or("*");
    let mode = f.gamemode.as_deref().unwrap_or("*");
    let limit = f.limit.map_or("*".to_owned(), |n| n.to_string());
    format!("catalog:{source}:list:{prof}:{mode}:{limit}")
}

fn catalog_fetch_cache_key(source: &str, slug: &str) -> String {
    format!("catalog:{source}:build:{slug}")
}

/// Public so adapters can build canonical wiki URLs.
#[must_use]
pub fn wiki_page_url(title: &str) -> String {
    let encoded = url::form_urlencoded::byte_serialize(title.as_bytes()).collect::<String>();
    format!("https://wiki.guildwars2.com/wiki/{encoded}")
}

/// Snapshot of a character's build + equipment tabs at a moment in time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CharacterBuildSnapshot {
    pub character_name: String,
    pub build_tabs: Vec<serde_json::Value>,
    pub equipment_tabs: Vec<serde_json::Value>,
    pub fetched_at: DateTime<Utc>,
}

/// Generic per-id cache helper used by `get_skills` / `get_traits` /
/// `get_specializations`. Splits ids into cache hits + misses, fetches the
/// misses in one upstream call, writes them through, and merges.
///
/// `on_miss` is called *only* if there are any missing ids, with the full
/// list of misses — adapters get a single chunked request.
async fn cached_by_id<Id, T, KFn, MFn, MFut>(
    cache: &dyn Cache,
    ids: &[Id],
    key_for: KFn,
    on_miss: MFn,
) -> Result<BTreeMap<Id, T>, ServiceError>
where
    Id: Copy + Ord + std::fmt::Display,
    T: Clone + Serialize + for<'de> Deserialize<'de>,
    KFn: Fn(Id) -> String,
    MFn: FnOnce(Vec<Id>) -> MFut,
    MFut: std::future::Future<Output = Result<BTreeMap<Id, T>, ServiceError>>,
{
    let mut hits = BTreeMap::new();
    let mut misses = Vec::new();

    for id in ids {
        let k = key_for(*id);
        match cache.get(&k).await {
            Some(json) => match serde_json::from_str::<T>(&json) {
                Ok(v) => {
                    hits.insert(*id, v);
                }
                Err(e) => {
                    warn!(key = %k, error = ?e, "cache poisoned; refetching");
                    misses.push(*id);
                }
            },
            None => misses.push(*id),
        }
    }

    if !misses.is_empty() {
        let fetched = on_miss(misses).await?;
        for (id, item) in fetched {
            if let Ok(json) = serde_json::to_string(&item) {
                cache.set(&key_for(id), json, STATIC_TTL).await;
            }
            hits.insert(id, item);
        }
    }

    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_cache_key_uses_fingerprint() {
        let key = ApiKey::new(
            "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE-FFFFFFFF-GGGG-HHHH-IIII-JJJJJJJJJJJJ".to_owned(),
        )
        .unwrap();
        let cache_key = wallet_cache_key(&key);
        assert!(cache_key.starts_with("wallet:"));
        assert!(!cache_key.contains("AAAA"), "raw key leaked into cache key");
    }

    #[test]
    fn wiki_page_url_escapes_spaces() {
        let url = wiki_page_url("Dragon Bash");
        assert_eq!(url, "https://wiki.guildwars2.com/wiki/Dragon+Bash");
    }

    #[test]
    fn wiki_search_cache_key_includes_limit() {
        let q = SearchQuery::new("foo").unwrap();
        let l = SearchLimit::new(5).unwrap();
        assert_eq!(wiki_search_cache_key(&q, l), "wiki:search:foo:5");
    }
}
