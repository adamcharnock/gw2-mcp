//! Business logic. Orchestrates ports without ever knowing which concrete
//! adapter is plugged in. Caching policy lives here — adapters are dumb.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tracing::{debug, warn};

use crate::domain::{
    ApiKey, Currency, CurrencyId, SearchLimit, SearchQuery, SearchResponse, WalletEntry, WalletInfo,
};
use crate::ports::{Cache, CacheError, Clock, Gw2Api, Gw2ApiError, Wiki, WikiError};

// Cache TTLs centralised so changes are atomic.
pub const STATIC_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 365); // 1 year
pub const WIKI_TTL: Duration = Duration::from_secs(60 * 60 * 24); // 1 day
pub const WALLET_TTL: Duration = Duration::from_secs(5 * 60); // 5 minutes

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("upstream GW2 API error: {0}")]
    Gw2(#[from] Gw2ApiError),

    #[error("upstream wiki error: {0}")]
    Wiki(#[from] WikiError),

    #[error("cache error: {0}")]
    Cache(#[from] CacheError),
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
}

impl Service {
    pub fn new(
        gw2: Arc<dyn Gw2Api>,
        wiki: Arc<dyn Wiki>,
        cache: Arc<dyn Cache>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            gw2,
            wiki,
            cache,
            clock,
        }
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

/// Public so adapters can build canonical wiki URLs.
#[must_use]
pub fn wiki_page_url(title: &str) -> String {
    let encoded = url::form_urlencoded::byte_serialize(title.as_bytes()).collect::<String>();
    format!("https://wiki.guildwars2.com/wiki/{encoded}")
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
