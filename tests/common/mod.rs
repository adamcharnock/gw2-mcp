//! Shared test fixtures: a manually-advanceable clock and three in-memory
//! port fakes so service tests can exercise the real `Service` orchestrator
//! without HTTP, threads, or real time.

#![allow(dead_code)] // Each integration-test binary uses a different subset.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use gw2_mcp::domain::{
    ApiKey, Currency, CurrencyId, SearchLimit, SearchQuery, SearchResult, WalletEntry,
};
use gw2_mcp::ports::{Cache, CacheError, Clock, Gw2Api, Gw2ApiError, Wiki, WikiError};

// ---------------------------------------------------------------------------
// Test clock
// ---------------------------------------------------------------------------

/// Clock that starts at unix-epoch and only advances on `advance()`.
pub struct TestClock(Mutex<DateTime<Utc>>);

impl TestClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(
            DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
        )))
    }

    pub fn advance(&self, by: Duration) {
        let mut t = self.0.lock().unwrap();
        *t += chrono::Duration::from_std(by).unwrap();
    }
}

impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------
// In-memory cache for tests (no expiry — test clock controls service-level
// TTL semantics, but the cache just stores the value and a timestamp).
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CacheEntry {
    value: String,
    expires_at: DateTime<Utc>,
}

pub struct TestCache {
    inner: Mutex<BTreeMap<String, CacheEntry>>,
    clock: Arc<dyn Clock>,
    /// Total number of `set` calls — useful to assert "cache miss caused a fetch".
    pub set_count: Mutex<usize>,
    /// Total number of `get` calls.
    pub get_count: Mutex<usize>,
}

impl TestCache {
    pub fn new(clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BTreeMap::new()),
            clock,
            set_count: Mutex::new(0),
            get_count: Mutex::new(0),
        })
    }

    pub fn set_count(&self) -> usize {
        *self.set_count.lock().unwrap()
    }

    pub fn get_count(&self) -> usize {
        *self.get_count.lock().unwrap()
    }
}

#[async_trait]
impl Cache for TestCache {
    async fn set(&self, key: &str, value: String, ttl: Duration) {
        *self.set_count.lock().unwrap() += 1;
        let expires_at = self.clock.now()
            + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::days(365));
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_owned(), CacheEntry { value, expires_at });
    }

    async fn get(&self, key: &str) -> Option<String> {
        *self.get_count.lock().unwrap() += 1;
        let entry = self.inner.lock().unwrap().get(key).cloned()?;
        if self.clock.now() >= entry.expires_at {
            self.inner.lock().unwrap().remove(key);
            None
        } else {
            Some(entry.value)
        }
    }

    async fn delete(&self, key: &str) {
        self.inner.lock().unwrap().remove(key);
    }

    async fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

// ---------------------------------------------------------------------------
// Recording fake for the GW2 API port.
//
// The `responses` map gives the test seed data; `wallet_calls` etc. let the
// test assert the orchestrator did or did not hit the upstream.
// ---------------------------------------------------------------------------

pub struct FakeGw2Api {
    pub wallet_calls: Mutex<usize>,
    pub currency_calls: Mutex<usize>,
    pub wallet_response: Mutex<Result<Vec<WalletEntry>, Gw2ApiError>>,
    pub currencies: Mutex<BTreeMap<CurrencyId, Currency>>,
}

impl FakeGw2Api {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            wallet_calls: Mutex::new(0),
            currency_calls: Mutex::new(0),
            wallet_response: Mutex::new(Ok(Vec::new())),
            currencies: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn set_wallet(&self, entries: Vec<WalletEntry>) {
        *self.wallet_response.lock().unwrap() = Ok(entries);
    }

    pub fn set_wallet_unauthorized(&self) {
        *self.wallet_response.lock().unwrap() = Err(Gw2ApiError::Unauthorized);
    }

    pub fn add_currency(&self, c: Currency) {
        self.currencies.lock().unwrap().insert(c.id, c);
    }

    pub fn wallet_calls(&self) -> usize {
        *self.wallet_calls.lock().unwrap()
    }
    pub fn currency_calls(&self) -> usize {
        *self.currency_calls.lock().unwrap()
    }
}

#[async_trait]
impl Gw2Api for FakeGw2Api {
    async fn fetch_wallet(&self, _key: &ApiKey) -> Result<Vec<WalletEntry>, Gw2ApiError> {
        *self.wallet_calls.lock().unwrap() += 1;
        // Clone the result; cloning Gw2ApiError is non-trivial so map-error.
        match &*self.wallet_response.lock().unwrap() {
            Ok(v) => Ok(v.clone()),
            Err(Gw2ApiError::Unauthorized) => Err(Gw2ApiError::Unauthorized),
            Err(e) => Err(Gw2ApiError::Transport(format!("fake: {e}"))),
        }
    }

    async fn fetch_currency_ids(&self) -> Result<Vec<CurrencyId>, Gw2ApiError> {
        *self.currency_calls.lock().unwrap() += 1;
        Ok(self.currencies.lock().unwrap().keys().copied().collect())
    }

    async fn fetch_currencies(
        &self,
        ids: &[CurrencyId],
    ) -> Result<BTreeMap<CurrencyId, Currency>, Gw2ApiError> {
        *self.currency_calls.lock().unwrap() += 1;
        let store = self.currencies.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id).map(|c| (*id, c.clone())))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Recording fake for the wiki port.
// ---------------------------------------------------------------------------

pub struct FakeWiki {
    pub search_calls: Mutex<usize>,
    pub extract_calls: Mutex<usize>,
    pub results: Mutex<Vec<SearchResult>>,
    pub extracts: Mutex<BTreeMap<String, String>>,
}

impl FakeWiki {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            search_calls: Mutex::new(0),
            extract_calls: Mutex::new(0),
            results: Mutex::new(Vec::new()),
            extracts: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn set_search_results(&self, results: Vec<SearchResult>) {
        *self.results.lock().unwrap() = results;
    }

    pub fn set_extract(&self, title: &str, extract: &str) {
        self.extracts
            .lock()
            .unwrap()
            .insert(title.to_owned(), extract.to_owned());
    }

    pub fn search_calls(&self) -> usize {
        *self.search_calls.lock().unwrap()
    }

    pub fn extract_calls(&self) -> usize {
        *self.extract_calls.lock().unwrap()
    }
}

#[async_trait]
impl Wiki for FakeWiki {
    async fn search(
        &self,
        _query: &SearchQuery,
        _limit: SearchLimit,
    ) -> Result<Vec<SearchResult>, WikiError> {
        *self.search_calls.lock().unwrap() += 1;
        Ok(self.results.lock().unwrap().clone())
    }

    async fn fetch_extract(&self, title: &str) -> Result<String, WikiError> {
        *self.extract_calls.lock().unwrap() += 1;
        Ok(self
            .extracts
            .lock()
            .unwrap()
            .get(title)
            .cloned()
            .unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[must_use]
pub fn valid_api_key() -> ApiKey {
    ApiKey::new(
        "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE-FFFFFFFF-GGGG-HHHH-IIII-JJJJJJJJJJJJ".to_owned(),
    )
    .unwrap()
}

#[must_use]
pub fn currency(id: u32, name: &str) -> Currency {
    Currency {
        id: CurrencyId::new(i64::from(id)).unwrap(),
        name: name.to_owned(),
        description: format!("description for {name}"),
        icon: format!("https://render.example/{name}.png"),
        order: i32::try_from(id).unwrap_or(0),
    }
}

// Suppress unused-warning when an integration test binary doesn't use this re-export.
#[allow(unused_imports)]
pub use gw2_mcp::service::Service;

#[allow(unused_imports)]
pub use std::sync::Arc as TestArc;

// CacheError is referenced indirectly via the Cache port; keep imported so any
// future test that needs the error type doesn't have to re-import.
#[allow(dead_code)]
type _CacheError = CacheError;
