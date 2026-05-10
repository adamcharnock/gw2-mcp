//! Shared test fixtures: a manually-advanceable clock and three in-memory
//! port fakes so service tests can exercise the real `Service` orchestrator
//! without HTTP, threads, or real time.

#![allow(dead_code)] // Each integration-test binary uses a different subset.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use gw2_mcp::adapters::ChatrDecoder;
use gw2_mcp::domain::{
    Achievement, AchievementId, ApiKey, BuildSlug, CharacterName, Currency, CurrencyId, Item,
    ItemId, SearchLimit, SearchQuery, SearchResult, Skill, SkillId, Specialization,
    SpecializationId, Trait, TraitId, WalletEntry,
};
use gw2_mcp::ports::{
    BuildCatalog, BuildCodeDecoder, BuildDetail, BuildSummary, Cache, CacheError, CatalogError,
    CatalogFilter, CatalogRegistry, Clock, Gw2Api, Gw2ApiError, Wiki, WikiError,
};

/// Build a `Service` with the in-memory fakes and a real chatr decoder.
/// Tests that need a different decoder can call `Service::new` directly.
#[must_use]
pub fn build_service(
    gw2: Arc<dyn Gw2Api>,
    wiki: Arc<dyn Wiki>,
    cache: Arc<dyn Cache>,
    clock: Arc<dyn Clock>,
) -> Service {
    let decoder: Arc<dyn BuildCodeDecoder> = Arc::new(ChatrDecoder);
    let catalogs = Arc::new(CatalogRegistry::new());
    Service::new(gw2, wiki, cache, clock, decoder, catalogs)
}

/// Build a `Service` with a custom set of catalogs registered.
#[must_use]
pub fn build_service_with_catalogs(
    gw2: Arc<dyn Gw2Api>,
    wiki: Arc<dyn Wiki>,
    cache: Arc<dyn Cache>,
    clock: Arc<dyn Clock>,
    catalogs: Arc<CatalogRegistry>,
) -> Service {
    let decoder: Arc<dyn BuildCodeDecoder> = Arc::new(ChatrDecoder);
    Service::new(gw2, wiki, cache, clock, decoder, catalogs)
}

// ---------------------------------------------------------------------------
// Recording fake for the BuildCatalog port. The service caches catalog
// calls — the call counters here let tests prove cache hits and misses.
// ---------------------------------------------------------------------------

pub struct FakeCatalog {
    name: &'static str,
    pub list_calls: Mutex<usize>,
    pub fetch_calls: Mutex<usize>,
    pub list_response: Mutex<Vec<BuildSummary>>,
    pub fetch_response: Mutex<Option<BuildDetail>>,
}

impl FakeCatalog {
    pub fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            list_calls: Mutex::new(0),
            fetch_calls: Mutex::new(0),
            list_response: Mutex::new(Vec::new()),
            fetch_response: Mutex::new(None),
        })
    }

    pub fn set_list(&self, summaries: Vec<BuildSummary>) {
        *self.list_response.lock().unwrap() = summaries;
    }

    pub fn set_fetch(&self, detail: BuildDetail) {
        *self.fetch_response.lock().unwrap() = Some(detail);
    }

    pub fn list_calls(&self) -> usize {
        *self.list_calls.lock().unwrap()
    }

    pub fn fetch_calls(&self) -> usize {
        *self.fetch_calls.lock().unwrap()
    }
}

#[async_trait]
impl BuildCatalog for FakeCatalog {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn list(&self, _filter: &CatalogFilter) -> Result<Vec<BuildSummary>, CatalogError> {
        *self.list_calls.lock().unwrap() += 1;
        Ok(self.list_response.lock().unwrap().clone())
    }

    async fn fetch(&self, slug: &BuildSlug) -> Result<BuildDetail, CatalogError> {
        *self.fetch_calls.lock().unwrap() += 1;
        match self.fetch_response.lock().unwrap().clone() {
            Some(d) => Ok(d),
            None => Err(CatalogError::NotFound {
                source_name: self.name.to_owned(),
                slug: slug.as_str().to_owned(),
            }),
        }
    }
}

#[must_use]
pub fn build_summary(slug: &str, profession: &str) -> BuildSummary {
    BuildSummary {
        slug: slug.to_owned(),
        title: slug.to_owned(),
        profession: profession.to_owned(),
        elite_spec: None,
        role: String::new(),
        gamemode: "fractals".to_owned(),
        rating: None,
        source: "fake".to_owned(),
        source_url: format!("https://example.test/{slug}"),
    }
}

#[must_use]
pub fn build_detail(slug: &str, profession: &str) -> BuildDetail {
    BuildDetail {
        summary: build_summary(slug, profession),
        details: serde_json::Value::Null,
        description: String::new(),
        chat_code: None,
    }
}

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
    pub skill_calls: Mutex<usize>,
    pub trait_calls: Mutex<usize>,
    pub spec_calls: Mutex<usize>,
    pub item_calls: Mutex<usize>,
    pub achievement_calls: Mutex<usize>,
    pub build_calls: Mutex<usize>,
    pub buildtab_calls: Mutex<usize>,
    pub equipmenttab_calls: Mutex<usize>,
    pub wallet_response: Mutex<Result<Vec<WalletEntry>, Gw2ApiError>>,
    pub currencies: Mutex<BTreeMap<CurrencyId, Currency>>,
    pub skills: Mutex<BTreeMap<SkillId, Skill>>,
    pub traits: Mutex<BTreeMap<TraitId, Trait>>,
    pub specs: Mutex<BTreeMap<SpecializationId, Specialization>>,
    pub items: Mutex<BTreeMap<ItemId, Item>>,
    pub achievements: Mutex<BTreeMap<AchievementId, Achievement>>,
    pub build_number: Mutex<u32>,
    pub buildtabs: Mutex<BTreeMap<String, Vec<serde_json::Value>>>,
    pub equipmenttabs: Mutex<BTreeMap<String, Vec<serde_json::Value>>>,
}

impl FakeGw2Api {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            wallet_calls: Mutex::new(0),
            currency_calls: Mutex::new(0),
            skill_calls: Mutex::new(0),
            trait_calls: Mutex::new(0),
            spec_calls: Mutex::new(0),
            item_calls: Mutex::new(0),
            achievement_calls: Mutex::new(0),
            build_calls: Mutex::new(0),
            buildtab_calls: Mutex::new(0),
            equipmenttab_calls: Mutex::new(0),
            wallet_response: Mutex::new(Ok(Vec::new())),
            currencies: Mutex::new(BTreeMap::new()),
            skills: Mutex::new(BTreeMap::new()),
            traits: Mutex::new(BTreeMap::new()),
            specs: Mutex::new(BTreeMap::new()),
            items: Mutex::new(BTreeMap::new()),
            achievements: Mutex::new(BTreeMap::new()),
            build_number: Mutex::new(123_456),
            buildtabs: Mutex::new(BTreeMap::new()),
            equipmenttabs: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn add_achievement(&self, a: Achievement) {
        self.achievements.lock().unwrap().insert(a.id, a);
    }

    pub fn set_build_number(&self, n: u32) {
        *self.build_number.lock().unwrap() = n;
    }

    pub fn build_calls(&self) -> usize {
        *self.build_calls.lock().unwrap()
    }

    pub fn achievement_calls(&self) -> usize {
        *self.achievement_calls.lock().unwrap()
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

    pub fn add_skill(&self, s: Skill) {
        self.skills.lock().unwrap().insert(s.id, s);
    }

    pub fn add_trait(&self, t: Trait) {
        self.traits.lock().unwrap().insert(t.id, t);
    }

    pub fn add_specialization(&self, s: Specialization) {
        self.specs.lock().unwrap().insert(s.id, s);
    }

    pub fn add_item(&self, i: Item) {
        self.items.lock().unwrap().insert(i.id, i);
    }

    pub fn set_buildtabs(&self, name: &CharacterName, tabs: Vec<serde_json::Value>) {
        self.buildtabs
            .lock()
            .unwrap()
            .insert(name.as_str().to_owned(), tabs);
    }

    pub fn set_equipmenttabs(&self, name: &CharacterName, tabs: Vec<serde_json::Value>) {
        self.equipmenttabs
            .lock()
            .unwrap()
            .insert(name.as_str().to_owned(), tabs);
    }

    pub fn wallet_calls(&self) -> usize {
        *self.wallet_calls.lock().unwrap()
    }
    pub fn currency_calls(&self) -> usize {
        *self.currency_calls.lock().unwrap()
    }
    pub fn skill_calls(&self) -> usize {
        *self.skill_calls.lock().unwrap()
    }
    pub fn buildtab_calls(&self) -> usize {
        *self.buildtab_calls.lock().unwrap()
    }
}

#[async_trait]
impl Gw2Api for FakeGw2Api {
    async fn fetch_wallet(&self, _key: &ApiKey) -> Result<Vec<WalletEntry>, Gw2ApiError> {
        *self.wallet_calls.lock().unwrap() += 1;
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

    async fn fetch_skills(&self, ids: &[SkillId]) -> Result<BTreeMap<SkillId, Skill>, Gw2ApiError> {
        *self.skill_calls.lock().unwrap() += 1;
        let store = self.skills.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id).map(|s| (*id, s.clone())))
            .collect())
    }

    async fn fetch_traits(&self, ids: &[TraitId]) -> Result<BTreeMap<TraitId, Trait>, Gw2ApiError> {
        *self.trait_calls.lock().unwrap() += 1;
        let store = self.traits.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id).map(|t| (*id, t.clone())))
            .collect())
    }

    async fn fetch_specializations(
        &self,
        ids: &[SpecializationId],
    ) -> Result<BTreeMap<SpecializationId, Specialization>, Gw2ApiError> {
        *self.spec_calls.lock().unwrap() += 1;
        let store = self.specs.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id).map(|s| (*id, s.clone())))
            .collect())
    }

    async fn fetch_items(&self, ids: &[ItemId]) -> Result<BTreeMap<ItemId, Item>, Gw2ApiError> {
        *self.item_calls.lock().unwrap() += 1;
        let store = self.items.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id).map(|i| (*id, i.clone())))
            .collect())
    }

    async fn fetch_achievements(
        &self,
        ids: &[AchievementId],
    ) -> Result<BTreeMap<AchievementId, Achievement>, Gw2ApiError> {
        *self.achievement_calls.lock().unwrap() += 1;
        let store = self.achievements.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id).map(|a| (*id, a.clone())))
            .collect())
    }

    async fn fetch_all_skill_ids(&self) -> Result<Vec<SkillId>, Gw2ApiError> {
        Ok(self.skills.lock().unwrap().keys().copied().collect())
    }

    async fn fetch_all_trait_ids(&self) -> Result<Vec<TraitId>, Gw2ApiError> {
        Ok(self.traits.lock().unwrap().keys().copied().collect())
    }

    async fn fetch_all_specialization_ids(&self) -> Result<Vec<SpecializationId>, Gw2ApiError> {
        Ok(self.specs.lock().unwrap().keys().copied().collect())
    }

    async fn fetch_all_item_ids(&self) -> Result<Vec<ItemId>, Gw2ApiError> {
        Ok(self.items.lock().unwrap().keys().copied().collect())
    }

    async fn fetch_all_achievement_ids(&self) -> Result<Vec<AchievementId>, Gw2ApiError> {
        Ok(self.achievements.lock().unwrap().keys().copied().collect())
    }

    async fn fetch_build(&self) -> Result<u32, Gw2ApiError> {
        *self.build_calls.lock().unwrap() += 1;
        Ok(*self.build_number.lock().unwrap())
    }

    async fn fetch_buildtabs(
        &self,
        _key: &ApiKey,
        name: &CharacterName,
    ) -> Result<Vec<serde_json::Value>, Gw2ApiError> {
        *self.buildtab_calls.lock().unwrap() += 1;
        Ok(self
            .buildtabs
            .lock()
            .unwrap()
            .get(name.as_str())
            .cloned()
            .unwrap_or_default())
    }

    async fn fetch_equipmenttabs(
        &self,
        _key: &ApiKey,
        name: &CharacterName,
    ) -> Result<Vec<serde_json::Value>, Gw2ApiError> {
        *self.equipmenttab_calls.lock().unwrap() += 1;
        Ok(self
            .equipmenttabs
            .lock()
            .unwrap()
            .get(name.as_str())
            .cloned()
            .unwrap_or_default())
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
