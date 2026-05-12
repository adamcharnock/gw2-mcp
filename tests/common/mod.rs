//! Shared test fixtures: a manually-advanceable clock and three in-memory
//! port fakes so service tests can exercise the real `Service` orchestrator
//! without HTTP, threads, or real time.

#![allow(dead_code)] // Each integration-test binary uses a different subset.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use gw2_mcp::adapters::{ChatrDecoder, StubMumbleLink};
use gw2_mcp::domain::{
    Account, AccountAchievement, AccountMastery, Achievement, AchievementId, ApiKey, BuildSlug,
    CharacterName, Currency, CurrencyId, Dungeon, Item, ItemId, Mastery, MasteryId, Raid, Region,
    SearchLimit, SearchQuery, SearchResult, Skill, SkillId, Specialization, SpecializationId,
    Trait, TraitId, WalletEntry, WizardsVaultTrack,
};
use gw2_mcp::ports::{
    BuildCatalog, BuildCodeDecoder, BuildDetail, BuildSummary, Cache, CacheError, CatalogError,
    CatalogFilter, CatalogRegistry, Clock, Gw2Api, Gw2ApiError, MapData, MapDataError, MapId,
    MapInfo, MapPoi, MumbleContext, MumbleError, MumbleIdentity, MumbleLink, MumbleSnapshot, Wiki,
    WikiError,
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
    let mumble: Arc<dyn MumbleLink> =
        Arc::new(StubMumbleLink::new("test default: no mumble link wired"));
    let maps: Arc<dyn MapData> = Arc::new(FakeMapData::new());
    Service::new(gw2, wiki, cache, clock, decoder, catalogs, mumble, maps)
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
    let mumble: Arc<dyn MumbleLink> =
        Arc::new(StubMumbleLink::new("test default: no mumble link wired"));
    let maps: Arc<dyn MapData> = Arc::new(FakeMapData::new());
    Service::new(gw2, wiki, cache, clock, decoder, catalogs, mumble, maps)
}

/// Build a `Service` with custom navigation ports — used by the
/// navigation-specific test binary so it can inject a `FakeMumbleLink`
/// and a seeded `FakeMapData` without sacrificing the catalog wiring.
#[must_use]
pub fn build_service_with_navigation(
    gw2: Arc<dyn Gw2Api>,
    wiki: Arc<dyn Wiki>,
    cache: Arc<dyn Cache>,
    clock: Arc<dyn Clock>,
    mumble: Arc<dyn MumbleLink>,
    maps: Arc<dyn MapData>,
) -> Service {
    let decoder: Arc<dyn BuildCodeDecoder> = Arc::new(ChatrDecoder);
    let catalogs = Arc::new(CatalogRegistry::new());
    Service::new(gw2, wiki, cache, clock, decoder, catalogs, mumble, maps)
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
    pub account_calls: Mutex<usize>,
    pub characters_list_calls: Mutex<usize>,
    pub achievements_calls: Mutex<usize>,
    pub masteries_calls: Mutex<usize>,
    pub raids_calls: Mutex<usize>,
    pub dungeons_calls: Mutex<usize>,
    pub wallet_response: Mutex<Result<Vec<WalletEntry>, Gw2ApiError>>,
    pub currencies: Mutex<BTreeMap<CurrencyId, Currency>>,
    pub skills: Mutex<BTreeMap<SkillId, Skill>>,
    pub traits: Mutex<BTreeMap<TraitId, Trait>>,
    pub specs: Mutex<BTreeMap<SpecializationId, Specialization>>,
    pub items: Mutex<BTreeMap<ItemId, Item>>,
    pub achievements: Mutex<BTreeMap<AchievementId, Achievement>>,
    pub mastery_metadata: Mutex<BTreeMap<MasteryId, Mastery>>,
    pub mastery_meta_calls: Mutex<usize>,
    pub raid_metadata: Mutex<BTreeMap<String, Raid>>,
    pub raid_meta_calls: Mutex<usize>,
    pub dungeon_metadata: Mutex<BTreeMap<String, Dungeon>>,
    pub dungeon_meta_calls: Mutex<usize>,
    pub build_number: Mutex<u32>,
    pub buildtabs: Mutex<BTreeMap<String, Vec<serde_json::Value>>>,
    pub equipmenttabs: Mutex<BTreeMap<String, Vec<serde_json::Value>>>,
    pub account_response: Mutex<Option<Account>>,
    pub characters_list_response: Mutex<Vec<String>>,
    pub achievements_response: Mutex<Vec<AccountAchievement>>,
    pub masteries_response: Mutex<Vec<AccountMastery>>,
    pub mastery_points_response: Mutex<gw2_mcp::domain::AccountMasteryPoints>,
    pub bank_response: Mutex<Vec<gw2_mcp::domain::InventorySlot>>,
    pub materials_response: Mutex<Vec<gw2_mcp::domain::MaterialSlot>>,
    pub material_categories: Mutex<BTreeMap<u32, gw2_mcp::domain::MaterialCategory>>,
    pub character_inventory_response: Mutex<gw2_mcp::domain::CharacterInventory>,
    pub market_prices: Mutex<BTreeMap<ItemId, gw2_mcp::domain::MarketPrice>>,
    pub raids_response: Mutex<Vec<String>>,
    pub dungeons_response: Mutex<Vec<String>>,
    pub wizards_vault_daily: Mutex<WizardsVaultTrack>,
    pub wizards_vault_weekly: Mutex<WizardsVaultTrack>,
    pub wizards_vault_special: Mutex<WizardsVaultTrack>,
    pub wizards_vault_calls: Mutex<usize>,
    /// Keyed by `(continent_id, floor_id)` → `region_id → Region`.
    pub regions_on_floor: Mutex<BTreeMap<(u32, u32), BTreeMap<u32, Region>>>,
    /// Set of `(continent_id, floor_id)` pairs that should return an
    /// upstream error instead of the stored regions. Lets tests simulate
    /// "GW2 continents API unreachable" failure modes.
    pub regions_on_floor_errors: Mutex<BTreeSet<(u32, u32)>>,
    pub regions_calls: Mutex<usize>,
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
            account_calls: Mutex::new(0),
            characters_list_calls: Mutex::new(0),
            achievements_calls: Mutex::new(0),
            masteries_calls: Mutex::new(0),
            raids_calls: Mutex::new(0),
            dungeons_calls: Mutex::new(0),
            wallet_response: Mutex::new(Ok(Vec::new())),
            currencies: Mutex::new(BTreeMap::new()),
            skills: Mutex::new(BTreeMap::new()),
            traits: Mutex::new(BTreeMap::new()),
            specs: Mutex::new(BTreeMap::new()),
            items: Mutex::new(BTreeMap::new()),
            achievements: Mutex::new(BTreeMap::new()),
            mastery_metadata: Mutex::new(BTreeMap::new()),
            mastery_meta_calls: Mutex::new(0),
            raid_metadata: Mutex::new(BTreeMap::new()),
            raid_meta_calls: Mutex::new(0),
            dungeon_metadata: Mutex::new(BTreeMap::new()),
            dungeon_meta_calls: Mutex::new(0),
            build_number: Mutex::new(123_456),
            buildtabs: Mutex::new(BTreeMap::new()),
            equipmenttabs: Mutex::new(BTreeMap::new()),
            account_response: Mutex::new(None),
            characters_list_response: Mutex::new(Vec::new()),
            achievements_response: Mutex::new(Vec::new()),
            masteries_response: Mutex::new(Vec::new()),
            mastery_points_response: Mutex::new(gw2_mcp::domain::AccountMasteryPoints {
                totals: Vec::new(),
                unlocked: Vec::new(),
            }),
            bank_response: Mutex::new(Vec::new()),
            materials_response: Mutex::new(Vec::new()),
            material_categories: Mutex::new(BTreeMap::new()),
            character_inventory_response: Mutex::new(gw2_mcp::domain::CharacterInventory {
                bags: Vec::new(),
            }),
            market_prices: Mutex::new(BTreeMap::new()),
            raids_response: Mutex::new(Vec::new()),
            dungeons_response: Mutex::new(Vec::new()),
            wizards_vault_daily: Mutex::new(default_vault_track()),
            wizards_vault_weekly: Mutex::new(default_vault_track()),
            wizards_vault_special: Mutex::new(default_vault_track()),
            wizards_vault_calls: Mutex::new(0),
            regions_on_floor: Mutex::new(BTreeMap::new()),
            regions_on_floor_errors: Mutex::new(BTreeSet::new()),
            regions_calls: Mutex::new(0),
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

    pub fn set_bank(&self, slots: Vec<gw2_mcp::domain::InventorySlot>) {
        *self.bank_response.lock().unwrap() = slots;
    }

    pub fn set_materials(&self, slots: Vec<gw2_mcp::domain::MaterialSlot>) {
        *self.materials_response.lock().unwrap() = slots;
    }

    pub fn add_material_category(&self, cat: gw2_mcp::domain::MaterialCategory) {
        self.material_categories.lock().unwrap().insert(cat.id, cat);
    }

    pub fn set_character_inventory(&self, inv: gw2_mcp::domain::CharacterInventory) {
        *self.character_inventory_response.lock().unwrap() = inv;
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
    pub fn account_calls(&self) -> usize {
        *self.account_calls.lock().unwrap()
    }
    pub fn characters_list_calls(&self) -> usize {
        *self.characters_list_calls.lock().unwrap()
    }
    pub fn achievements_calls(&self) -> usize {
        *self.achievements_calls.lock().unwrap()
    }
    pub fn masteries_calls(&self) -> usize {
        *self.masteries_calls.lock().unwrap()
    }
    pub fn raids_calls(&self) -> usize {
        *self.raids_calls.lock().unwrap()
    }
    pub fn dungeons_calls(&self) -> usize {
        *self.dungeons_calls.lock().unwrap()
    }
    pub fn wizards_vault_calls(&self) -> usize {
        *self.wizards_vault_calls.lock().unwrap()
    }

    pub fn set_wizards_vault_daily(&self, t: WizardsVaultTrack) {
        *self.wizards_vault_daily.lock().unwrap() = t;
    }
    pub fn set_wizards_vault_weekly(&self, t: WizardsVaultTrack) {
        *self.wizards_vault_weekly.lock().unwrap() = t;
    }
    pub fn set_wizards_vault_special(&self, t: WizardsVaultTrack) {
        *self.wizards_vault_special.lock().unwrap() = t;
    }

    pub fn set_account(&self, acc: Account) {
        *self.account_response.lock().unwrap() = Some(acc);
    }
    pub fn set_characters_list(&self, names: Vec<String>) {
        *self.characters_list_response.lock().unwrap() = names;
    }
    pub fn set_achievements(&self, items: Vec<AccountAchievement>) {
        *self.achievements_response.lock().unwrap() = items;
    }
    pub fn set_masteries(&self, items: Vec<AccountMastery>) {
        *self.masteries_response.lock().unwrap() = items;
    }
    pub fn set_raids(&self, items: Vec<String>) {
        *self.raids_response.lock().unwrap() = items;
    }
    pub fn set_dungeons(&self, items: Vec<String>) {
        *self.dungeons_response.lock().unwrap() = items;
    }
    pub fn set_regions_on_floor(
        &self,
        continent_id: u32,
        floor_id: u32,
        regions: BTreeMap<u32, Region>,
    ) {
        self.regions_on_floor
            .lock()
            .unwrap()
            .insert((continent_id, floor_id), regions);
    }
    /// Make this `(continent_id, floor_id)` pair return a transport
    /// error from `fetch_regions_on_floor`, simulating an upstream
    /// outage.
    pub fn set_regions_on_floor_error(&self, continent_id: u32, floor_id: u32) {
        self.regions_on_floor_errors
            .lock()
            .unwrap()
            .insert((continent_id, floor_id));
    }
    pub fn regions_calls(&self) -> usize {
        *self.regions_calls.lock().unwrap()
    }
}

fn default_vault_track() -> WizardsVaultTrack {
    WizardsVaultTrack {
        meta_progress_current: 0,
        meta_progress_complete: 0,
        meta_reward_item_id: None,
        meta_reward_astral: 0,
        meta_reward_claimed: false,
        objectives: Vec::new(),
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

    async fn fetch_account(&self, _key: &ApiKey) -> Result<Account, Gw2ApiError> {
        *self.account_calls.lock().unwrap() += 1;
        self.account_response
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Gw2ApiError::Decode("no fake account configured".to_owned()))
    }

    async fn fetch_characters_list(&self, _key: &ApiKey) -> Result<Vec<String>, Gw2ApiError> {
        *self.characters_list_calls.lock().unwrap() += 1;
        Ok(self.characters_list_response.lock().unwrap().clone())
    }

    async fn fetch_account_achievements(
        &self,
        _key: &ApiKey,
    ) -> Result<Vec<AccountAchievement>, Gw2ApiError> {
        *self.achievements_calls.lock().unwrap() += 1;
        Ok(self.achievements_response.lock().unwrap().clone())
    }

    async fn fetch_account_masteries(
        &self,
        _key: &ApiKey,
    ) -> Result<Vec<AccountMastery>, Gw2ApiError> {
        *self.masteries_calls.lock().unwrap() += 1;
        Ok(self.masteries_response.lock().unwrap().clone())
    }

    async fn fetch_account_mastery_points(
        &self,
        _key: &ApiKey,
    ) -> Result<gw2_mcp::domain::AccountMasteryPoints, Gw2ApiError> {
        Ok(self.mastery_points_response.lock().unwrap().clone())
    }

    async fn fetch_account_bank(
        &self,
        _key: &ApiKey,
    ) -> Result<Vec<gw2_mcp::domain::InventorySlot>, Gw2ApiError> {
        Ok(self.bank_response.lock().unwrap().clone())
    }

    async fn fetch_account_materials(
        &self,
        _key: &ApiKey,
    ) -> Result<Vec<gw2_mcp::domain::MaterialSlot>, Gw2ApiError> {
        Ok(self.materials_response.lock().unwrap().clone())
    }

    async fn fetch_all_material_category_ids(&self) -> Result<Vec<u32>, Gw2ApiError> {
        Ok(self
            .material_categories
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect())
    }

    async fn fetch_material_categories(
        &self,
        ids: &[u32],
    ) -> Result<BTreeMap<u32, gw2_mcp::domain::MaterialCategory>, Gw2ApiError> {
        let table = self.material_categories.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| table.get(id).map(|c| (*id, c.clone())))
            .collect())
    }

    async fn fetch_character_inventory(
        &self,
        _key: &ApiKey,
        _name: &CharacterName,
    ) -> Result<gw2_mcp::domain::CharacterInventory, Gw2ApiError> {
        Ok(self.character_inventory_response.lock().unwrap().clone())
    }

    async fn fetch_market_prices(
        &self,
        ids: &[ItemId],
    ) -> Result<BTreeMap<ItemId, gw2_mcp::domain::MarketPrice>, Gw2ApiError> {
        let table = self.market_prices.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| table.get(id).map(|p| (*id, p.clone())))
            .collect())
    }

    async fn fetch_all_mastery_ids(&self) -> Result<Vec<MasteryId>, Gw2ApiError> {
        Ok(self
            .mastery_metadata
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect())
    }

    async fn fetch_masteries(
        &self,
        ids: &[MasteryId],
    ) -> Result<BTreeMap<MasteryId, Mastery>, Gw2ApiError> {
        *self.mastery_meta_calls.lock().unwrap() += 1;
        let store = self.mastery_metadata.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id).map(|m| (*id, m.clone())))
            .collect())
    }

    async fn fetch_account_raids(&self, _key: &ApiKey) -> Result<Vec<String>, Gw2ApiError> {
        *self.raids_calls.lock().unwrap() += 1;
        Ok(self.raids_response.lock().unwrap().clone())
    }

    async fn fetch_all_raid_ids(&self) -> Result<Vec<String>, Gw2ApiError> {
        Ok(self.raid_metadata.lock().unwrap().keys().cloned().collect())
    }

    async fn fetch_raids(&self, ids: &[String]) -> Result<BTreeMap<String, Raid>, Gw2ApiError> {
        *self.raid_meta_calls.lock().unwrap() += 1;
        let store = self.raid_metadata.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id).map(|r| (id.clone(), r.clone())))
            .collect())
    }

    async fn fetch_account_dungeons(&self, _key: &ApiKey) -> Result<Vec<String>, Gw2ApiError> {
        *self.dungeons_calls.lock().unwrap() += 1;
        Ok(self.dungeons_response.lock().unwrap().clone())
    }

    async fn fetch_all_dungeon_ids(&self) -> Result<Vec<String>, Gw2ApiError> {
        Ok(self
            .dungeon_metadata
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect())
    }

    async fn fetch_dungeons(
        &self,
        ids: &[String],
    ) -> Result<BTreeMap<String, Dungeon>, Gw2ApiError> {
        *self.dungeon_meta_calls.lock().unwrap() += 1;
        let store = self.dungeon_metadata.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id).map(|d| (id.clone(), d.clone())))
            .collect())
    }

    async fn fetch_wizards_vault_daily(
        &self,
        _key: &ApiKey,
    ) -> Result<WizardsVaultTrack, Gw2ApiError> {
        *self.wizards_vault_calls.lock().unwrap() += 1;
        Ok(self.wizards_vault_daily.lock().unwrap().clone())
    }

    async fn fetch_wizards_vault_weekly(
        &self,
        _key: &ApiKey,
    ) -> Result<WizardsVaultTrack, Gw2ApiError> {
        *self.wizards_vault_calls.lock().unwrap() += 1;
        Ok(self.wizards_vault_weekly.lock().unwrap().clone())
    }

    async fn fetch_wizards_vault_special(
        &self,
        _key: &ApiKey,
    ) -> Result<WizardsVaultTrack, Gw2ApiError> {
        *self.wizards_vault_calls.lock().unwrap() += 1;
        Ok(self.wizards_vault_special.lock().unwrap().clone())
    }

    async fn fetch_regions_on_floor(
        &self,
        continent_id: u32,
        floor_id: u32,
    ) -> Result<BTreeMap<u32, Region>, Gw2ApiError> {
        *self.regions_calls.lock().unwrap() += 1;
        if self
            .regions_on_floor_errors
            .lock()
            .unwrap()
            .contains(&(continent_id, floor_id))
        {
            return Err(Gw2ApiError::Transport(format!(
                "fake: continents/{continent_id}/floors/{floor_id} unreachable"
            )));
        }
        Ok(self
            .regions_on_floor
            .lock()
            .unwrap()
            .get(&(continent_id, floor_id))
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
    }
}

// ---------------------------------------------------------------------------
// Recording fake for the MapData port — used by the navigation tests.
// ---------------------------------------------------------------------------

pub struct FakeMapData {
    pub map_calls: Mutex<usize>,
    pub poi_calls: Mutex<usize>,
    pub maps: Mutex<BTreeMap<MapId, MapInfo>>,
    pub pois: Mutex<BTreeMap<MapId, Vec<MapPoi>>>,
}

impl FakeMapData {
    pub fn new() -> Self {
        Self {
            map_calls: Mutex::new(0),
            poi_calls: Mutex::new(0),
            maps: Mutex::new(BTreeMap::new()),
            pois: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn add_map(&self, info: MapInfo) {
        self.maps.lock().unwrap().insert(info.id, info);
    }

    pub fn add_pois(&self, map_id: MapId, pois: Vec<MapPoi>) {
        self.pois.lock().unwrap().insert(map_id, pois);
    }

    pub fn map_calls(&self) -> usize {
        *self.map_calls.lock().unwrap()
    }

    pub fn poi_calls(&self) -> usize {
        *self.poi_calls.lock().unwrap()
    }
}

#[async_trait]
impl MapData for FakeMapData {
    async fn get_map(&self, id: MapId) -> Result<MapInfo, MapDataError> {
        *self.map_calls.lock().unwrap() += 1;
        self.maps
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(MapDataError::NotFound(id))
    }

    async fn list_pois(&self, map_id: MapId) -> Result<Vec<MapPoi>, MapDataError> {
        *self.poi_calls.lock().unwrap() += 1;
        Ok(self
            .pois
            .lock()
            .unwrap()
            .get(&map_id)
            .cloned()
            .unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Recording fake for the MumbleLink port. Tests build a snapshot in-line
// rather than crafting raw bytes — the parsing path is already exercised
// in the unit tests for `adapters::mumble_link`.
// ---------------------------------------------------------------------------

pub struct FakeMumbleLink {
    pub calls: Mutex<usize>,
    pub response: Mutex<Result<MumbleSnapshot, MumbleError>>,
}

impl FakeMumbleLink {
    pub fn with_snapshot(snap: MumbleSnapshot) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(0),
            response: Mutex::new(Ok(snap)),
        })
    }

    pub fn with_error(err: MumbleError) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(0),
            response: Mutex::new(Err(err)),
        })
    }

    pub fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

impl MumbleLink for FakeMumbleLink {
    fn snapshot(&self) -> Result<MumbleSnapshot, MumbleError> {
        *self.calls.lock().unwrap() += 1;
        match &*self.response.lock().unwrap() {
            Ok(s) => Ok(s.clone()),
            Err(e) => Err(clone_mumble_error(e)),
        }
    }
}

/// `MumbleError` is `#[non_exhaustive]`-style enum (no Clone derived).
/// Reproduce by variant so the fake can hand out fresh copies without
/// the trait object boxing dance.
fn clone_mumble_error(e: &MumbleError) -> MumbleError {
    match e {
        MumbleError::NotConnected(s) => MumbleError::NotConnected(s.clone()),
        MumbleError::Unsupported(s) => MumbleError::Unsupported(s.clone()),
        MumbleError::Io(io) => MumbleError::Io(std::io::Error::new(io.kind(), io.to_string())),
        MumbleError::Decode(s) => MumbleError::Decode(s.clone()),
    }
}

#[must_use]
pub fn make_mumble_snapshot(
    map_id: u32,
    player_x: f32,
    player_y: f32,
    facing: [f32; 3],
    character: &str,
    profession: u8,
) -> MumbleSnapshot {
    MumbleSnapshot {
        ui_version: 1,
        ui_tick: 100,
        avatar_position: [0.0, 0.0, 0.0],
        avatar_front: facing,
        camera_position: [0.0, 0.0, 0.0],
        camera_front: facing,
        identity: MumbleIdentity {
            name: Some(character.to_owned()),
            profession: Some(profession),
            spec: Some(0),
            race: Some(2),
            map_id: Some(map_id),
            world_id: Some(2202),
            team_color_id: Some(0),
            commander: Some(false),
            fov: Some(1.222),
            uisz: Some(1),
        },
        context: MumbleContext {
            server_address: [0; 28],
            map_id,
            map_type: 5,
            shard_id: 0,
            instance: 0,
            build_id: 162_000,
            ui_state: 0,
            compass_width: 256,
            compass_height: 256,
            compass_rotation: 0.0,
            player_x,
            player_y,
            map_center_x: 0.0,
            map_center_y: 0.0,
            map_scale: 1.0,
            process_id: 1234,
            mount_index: 0,
        },
    }
}

#[must_use]
pub fn make_map_info(id: MapId, name: &str) -> MapInfo {
    MapInfo {
        id,
        name: name.to_owned(),
        map_type: Some("Public".to_owned()),
        min_level: Some(1),
        max_level: Some(15),
        default_floor: 1,
        region_id: 4,
        region_name: "Kryta".to_owned(),
        continent_id: 1,
        continent_name: "Tyria".to_owned(),
        continent_rect: [[0.0, 0.0], [10000.0, 10000.0]],
        map_rect: [[0.0, 0.0], [20000.0, 20000.0]],
    }
}

#[must_use]
pub fn make_poi(id: u64, name: &str, kind: &str, coord: (f64, f64)) -> MapPoi {
    MapPoi {
        id,
        name: name.to_owned(),
        kind: kind.to_owned(),
        coord,
        floor: 1,
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
