//! Business logic. Orchestrates ports without ever knowing which concrete
//! adapter is plugged in. Caching policy lives here — adapters are dumb.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tracing::{debug, warn};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::adapters::mumble_link::{MumbleError, MumbleLink};
use crate::domain::BuildChatCode;
use crate::domain::bearing::{Bearing16, bearing, distance_meters, distance_units};
use crate::domain::{
    Account, AccountAchievement, AccountMastery, ApiKey, BuildSlug, CharacterName, Currency,
    CurrencyId, Dailies, Item, ItemId, SearchLimit, SearchQuery, SearchResponse, Skill, SkillId,
    Specialization, SpecializationId, Trait, TraitId, WalletEntry, WalletInfo,
};
use crate::ports::{
    AchievementRef, AchievementSearchFilter, BuildCodeDecoder, BuildCodeError, Cache, CacheError,
    Clock, Gw2Api, Gw2ApiError, IndexStatus, ItemRef, ItemSearchFilter, MapData, MapDataError,
    MapId, MapInfo, MapPoi, SearchError, SearchIndex, SkillRef, SkillSearchFilter, SpecRef,
    SpecSearchFilter, TraitRef, TraitSearchFilter, Wiki, WikiError,
};

// Cache TTLs centralised so changes are atomic.
pub const STATIC_TTL: Duration = Duration::from_secs(60 * 60 * 24 * 365); // 1 year
pub const WIKI_TTL: Duration = Duration::from_secs(60 * 60 * 24); // 1 day
pub const WALLET_TTL: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// Dailies roll over once per day. A 1-hour TTL keeps the worst case at
/// 24 fetches/day per server while still picking up the rollover within
/// an hour — short enough that the LLM never plans an obsolete routine
/// against last night's dailies.
pub const DAILIES_TTL: Duration = Duration::from_secs(60 * 60); // 1 hour

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

    #[error("{0}")]
    MapData(#[from] MapDataError),

    /// Mumble Link state was requested but isn't available — surfaces
    /// the platform-specific reason (no GW2 client running, headless
    /// container, etc.) verbatim so the LLM can quote it back.
    #[error("{0}")]
    Mumble(#[from] MumbleError),

    /// A `LocationRef::NamedPoi` referenced a name that didn't match
    /// any POI on the given map. Carries the failed name so the LLM
    /// can correct or ask the user.
    #[error(
        "no POI named `{name}` found on map id {map_id}. Try `find_nearby` first to enumerate \
         POIs on this map."
    )]
    NoSuchPoi { map_id: MapId, name: String },

    #[error("{0}")]
    Search(#[from] SearchError),

    /// The server was started with `--no-search-index`, so the search tools
    /// have no backing store at all. Distinct from [`SearchError::NotIndexed`]
    /// (which means "still populating") so the LLM gets a different
    /// recovery hint.
    #[error(
        "search index is disabled on this server (started with --no-search-index). Use the typed \
         get_* tools with explicit ids."
    )]
    SearchDisabled,
}

/// Which build/equipment tab(s) to project from a character snapshot.
///
/// Defaults to `Active` because that's the in-game-equipped build — what
/// "what is this character running?" almost always means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabSelector {
    /// The single tab marked active in-game.
    #[default]
    Active,
    /// All tabs (verbatim).
    All,
    /// A specific tab number (1-indexed, matching the GW2 API `tab` field).
    Index(u8),
}

/// Which day's dailies to fetch — `Today` hits `/v2/achievements/daily`,
/// `Tomorrow` hits `/v2/achievements/daily/tomorrow`. Defaults to today
/// because "what should I do?" almost always means right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DailiesWhich {
    #[default]
    Today,
    Tomorrow,
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
    /// Server-default API key. Tools that take an optional `api_key` arg
    /// fall back to this when the caller omits it. Set via the binary's
    /// `--api-key` flag / `GW2_API_KEY` env var.
    default_api_key: Option<ApiKey>,
    /// Live game-state reader. Plumbed in unconditionally; the binary
    /// wires a `StubMumbleLink` when no real reader is reachable so
    /// non-navigation tools keep working on headless hosts.
    mumble: Arc<dyn MumbleLink>,
    /// GW2 map / POI client.
    maps: Arc<dyn MapData>,
    /// Optional on-disk search index. `None` when started with
    /// `--no-search-index`, in which case the `search_*` methods all return
    /// [`ServiceError::SearchDisabled`].
    search_index: Option<Arc<dyn SearchIndex>>,
}

impl Service {
    // The constructor takes every port the service needs explicitly, so
    // callers (main.rs + tests) can see the full dependency surface at
    // wiring time. Refactoring into a builder buys little for the
    // friction it adds in the integration tests.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        gw2: Arc<dyn Gw2Api>,
        wiki: Arc<dyn Wiki>,
        cache: Arc<dyn Cache>,
        clock: Arc<dyn Clock>,
        build_decoder: Arc<dyn BuildCodeDecoder>,
        catalogs: Arc<crate::ports::CatalogRegistry>,
        mumble: Arc<dyn MumbleLink>,
        maps: Arc<dyn MapData>,
    ) -> Self {
        Self {
            gw2,
            wiki,
            cache,
            clock,
            build_decoder,
            catalogs,
            default_api_key: None,
            mumble,
            maps,
            search_index: None,
        }
    }

    /// Set the default API key used when MCP callers omit `api_key`.
    /// Builder-style so wiring stays a one-liner.
    #[must_use]
    pub fn with_default_api_key(mut self, key: ApiKey) -> Self {
        self.default_api_key = Some(key);
        self
    }

    /// Wire a search index in. `None` = `--no-search-index` mode.
    #[must_use]
    pub fn with_search_index(mut self, idx: Arc<dyn SearchIndex>) -> Self {
        self.search_index = Some(idx);
        self
    }

    fn search_index(&self) -> Result<&Arc<dyn SearchIndex>, ServiceError> {
        self.search_index
            .as_ref()
            .ok_or(ServiceError::SearchDisabled)
    }

    /// Run a fuzzy name search across the indexed skills corpus.
    pub async fn search_skills(
        &self,
        q: &str,
        limit: u32,
        filter: SkillSearchFilter,
    ) -> Result<Vec<SkillRef>, ServiceError> {
        Ok(self.search_index()?.search_skills(q, limit, filter).await?)
    }

    pub async fn search_traits(
        &self,
        q: &str,
        limit: u32,
        filter: TraitSearchFilter,
    ) -> Result<Vec<TraitRef>, ServiceError> {
        Ok(self.search_index()?.search_traits(q, limit, filter).await?)
    }

    pub async fn search_specializations(
        &self,
        q: &str,
        limit: u32,
        filter: SpecSearchFilter,
    ) -> Result<Vec<SpecRef>, ServiceError> {
        Ok(self
            .search_index()?
            .search_specializations(q, limit, filter)
            .await?)
    }

    pub async fn search_items(
        &self,
        q: &str,
        limit: u32,
        filter: ItemSearchFilter,
    ) -> Result<Vec<ItemRef>, ServiceError> {
        Ok(self.search_index()?.search_items(q, limit, filter).await?)
    }

    pub async fn search_achievements(
        &self,
        q: &str,
        limit: u32,
        filter: AchievementSearchFilter,
    ) -> Result<Vec<AchievementRef>, ServiceError> {
        Ok(self
            .search_index()?
            .search_achievements(q, limit, filter)
            .await?)
    }

    pub async fn get_index_status(&self) -> Result<IndexStatus, ServiceError> {
        Ok(self.search_index()?.index_status().await?)
    }

    /// Returns the server-default API key, if one was wired in.
    #[must_use]
    pub fn default_api_key(&self) -> Option<&ApiKey> {
        self.default_api_key.as_ref()
    }

    /// Decode a `[&Dw…]` build chat code into structured JSON.
    ///
    /// On top of the raw decoder output, this:
    /// - resolves trait *positions* (1..=3 column index per tier) into the
    ///   concrete `trait_id` from the specialisation's `major_traits` array,
    ///   so the LLM can pass the id straight into `get_traits`;
    /// - adds a `profession_name` next to the raw `profession` byte.
    ///
    /// Specialisation lookups go through the cached `get_specializations`
    /// path so repeated decodes for the same profession are cheap.
    pub async fn decode_build_code(&self, code: &BuildChatCode) -> Result<Value, ServiceError> {
        let mut value = self.build_decoder.decode(code)?;

        // Profession name (1-based byte → name).
        if let Some(prof) = value.get("profession").and_then(Value::as_u64)
            && let Ok(byte) = u8::try_from(prof)
            && let Some(name) = profession_byte_to_name(byte)
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert("profession_name".to_owned(), json!(name));
        }

        // Resolve trait column-positions to concrete trait_ids.
        // Collect the spec ids first so we batch the lookup.
        let spec_ids: Vec<SpecializationId> = value
            .get("specializations")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.get("id").and_then(Value::as_u64))
                    .filter_map(|id| SpecializationId::new(i64::try_from(id).ok()?).ok())
                    .collect()
            })
            .unwrap_or_default();

        let specs = if spec_ids.is_empty() {
            BTreeMap::new()
        } else {
            // Don't fail decode if upstream lookups fail — return the
            // structural-only output and let the LLM ask again.
            match self.get_specializations(&spec_ids).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(error = ?e, "decode_build_code: failed to resolve specializations; returning unresolved traits");
                    BTreeMap::new()
                }
            }
        };

        if let Some(arr) = value
            .get_mut("specializations")
            .and_then(Value::as_array_mut)
        {
            for spec_obj in arr.iter_mut() {
                let spec_id = spec_obj
                    .get("id")
                    .and_then(Value::as_u64)
                    .and_then(|n| SpecializationId::new(i64::try_from(n).ok()?).ok());
                let major_traits: Vec<u32> = spec_id
                    .and_then(|id| specs.get(&id))
                    .and_then(|s| s.extra.get("major_traits"))
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .map(|v| v.as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0))
                            .collect()
                    })
                    .unwrap_or_default();

                if let Some(traits_obj) = spec_obj.get_mut("traits").and_then(Value::as_object_mut)
                {
                    let new_obj: Map<String, Value> =
                        [("adept", 0u8), ("master", 1u8), ("grandmaster", 2u8)]
                            .into_iter()
                            .map(|(slot, tier)| {
                                let raw = traits_obj.get(slot).and_then(Value::as_u64).unwrap_or(0);
                                let position = u8::try_from(raw).unwrap_or(0);
                                let trait_id = if (1..=3).contains(&position) {
                                    let idx = usize::from(tier) * 3 + usize::from(position) - 1;
                                    major_traits.get(idx).copied().filter(|n| *n > 0)
                                } else {
                                    None
                                };
                                (
                                    slot.to_owned(),
                                    json!({
                                        "position": position,
                                        "trait_id": trait_id,
                                    }),
                                )
                            })
                            .collect();
                    *traits_obj = new_obj;
                }
            }
        }

        Ok(value)
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
        slug: &BuildSlug,
    ) -> Result<crate::ports::BuildDetail, ServiceError> {
        let cat = self
            .catalogs
            .get(source)
            .ok_or_else(|| crate::ports::CatalogError::NoSuchSource(source.to_owned()))?;

        let cache_key = catalog_fetch_cache_key(source, slug.as_str());
        if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(cached) = serde_json::from_str::<crate::ports::BuildDetail>(&json)
        {
            debug!(source, slug = slug.as_str(), "catalog fetch cache hit");
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
    // Tier 6A — account / progression / dailies
    //
    // All cached on `WALLET_TTL` (5 min) except `get_dailies` which uses
    // `DAILIES_TTL` (1 hour). Cache keys derive from `key.fingerprint()`,
    // never the raw key — the same pattern wallet/character_build use.
    // -----------------------------------------------------------------

    /// Fetch the account snapshot (`/v2/account`). Cached `WALLET_TTL`.
    pub async fn get_account(&self, key: &ApiKey) -> Result<Account, ServiceError> {
        let cache_key = account_cache_key(key);
        if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(acc) = serde_json::from_str::<Account>(&json)
        {
            debug!(fingerprint = %key.fingerprint(), "account cache hit");
            return Ok(acc);
        }
        let acc = self.gw2.fetch_account(key).await?;
        if let Ok(json) = serde_json::to_string(&acc) {
            self.cache.set(&cache_key, json, WALLET_TTL).await;
        }
        Ok(acc)
    }

    /// Fetch character names only (`/v2/characters`). Cached `WALLET_TTL`.
    pub async fn list_characters(&self, key: &ApiKey) -> Result<Vec<String>, ServiceError> {
        let cache_key = format!("characters:{}", key.fingerprint());
        if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str::<Vec<String>>(&json)
        {
            return Ok(v);
        }
        let v = self.gw2.fetch_characters_list(key).await?;
        if let Ok(json) = serde_json::to_string(&v) {
            self.cache.set(&cache_key, json, WALLET_TTL).await;
        }
        Ok(v)
    }

    /// Fetch the per-account achievement progress list. Cached `WALLET_TTL`.
    ///
    /// When `summary` is true (the LLM-friendly default), drops entries
    /// where the user is fully done (`done==true` or `current==max`) and
    /// entries where they haven't started (`current==0` or `current` is
    /// missing). What's left is the "what am I working on?" set — usually
    /// 100-300 entries instead of 2000-3000.
    pub async fn get_account_achievements(
        &self,
        key: &ApiKey,
        summary: bool,
    ) -> Result<Vec<AccountAchievement>, ServiceError> {
        let cache_key = format!("account_achievements:{}", key.fingerprint());
        let raw: Vec<AccountAchievement> = if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str(&json)
        {
            v
        } else {
            let v = self.gw2.fetch_account_achievements(key).await?;
            if let Ok(json) = serde_json::to_string(&v) {
                self.cache.set(&cache_key, json, WALLET_TTL).await;
            }
            v
        };
        if !summary {
            return Ok(raw);
        }
        Ok(raw
            .into_iter()
            .filter(|a| !a.is_completed() && !a.is_not_started())
            .collect())
    }

    /// Fetch unlocked-mastery progress per track. Cached `WALLET_TTL`.
    pub async fn get_account_masteries(
        &self,
        key: &ApiKey,
    ) -> Result<Vec<AccountMastery>, ServiceError> {
        let cache_key = format!("account_masteries:{}", key.fingerprint());
        if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str::<Vec<AccountMastery>>(&json)
        {
            return Ok(v);
        }
        let v = self.gw2.fetch_account_masteries(key).await?;
        if let Ok(json) = serde_json::to_string(&v) {
            self.cache.set(&cache_key, json, WALLET_TTL).await;
        }
        Ok(v)
    }

    /// Fetch raid encounter ids cleared this reset week. Cached `WALLET_TTL`.
    pub async fn get_account_raids(&self, key: &ApiKey) -> Result<Vec<String>, ServiceError> {
        let cache_key = format!("account_raids:{}", key.fingerprint());
        if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str::<Vec<String>>(&json)
        {
            return Ok(v);
        }
        let v = self.gw2.fetch_account_raids(key).await?;
        if let Ok(json) = serde_json::to_string(&v) {
            self.cache.set(&cache_key, json, WALLET_TTL).await;
        }
        Ok(v)
    }

    /// Fetch dungeon-path ids cleared today (resets daily). Cached `WALLET_TTL`.
    pub async fn get_account_dungeons(&self, key: &ApiKey) -> Result<Vec<String>, ServiceError> {
        let cache_key = format!("account_dungeons:{}", key.fingerprint());
        if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str::<Vec<String>>(&json)
        {
            return Ok(v);
        }
        let v = self.gw2.fetch_account_dungeons(key).await?;
        if let Ok(json) = serde_json::to_string(&v) {
            self.cache.set(&cache_key, json, WALLET_TTL).await;
        }
        Ok(v)
    }

    /// Fetch today's or tomorrow's dailies. Public endpoint — no key.
    /// Cached `DAILIES_TTL` (1 hour).
    pub async fn get_dailies(&self, which: DailiesWhich) -> Result<Dailies, ServiceError> {
        let cache_key = match which {
            DailiesWhich::Today => "dailies:today".to_owned(),
            DailiesWhich::Tomorrow => "dailies:tomorrow".to_owned(),
        };
        if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(d) = serde_json::from_str::<Dailies>(&json)
        {
            return Ok(d);
        }
        let d = self
            .gw2
            .fetch_dailies(matches!(which, DailiesWhich::Tomorrow))
            .await?;
        if let Ok(json) = serde_json::to_string(&d) {
            self.cache.set(&cache_key, json, DAILIES_TTL).await;
        }
        Ok(d)
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
    // Reference data: skills, traits, specializations, items
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

    pub async fn get_items(&self, ids: &[ItemId]) -> Result<BTreeMap<ItemId, Item>, ServiceError> {
        cached_by_id(
            self.cache.as_ref(),
            ids,
            |id| format!("item:{id}"),
            |missing| async move { self.gw2.fetch_items(&missing).await.map_err(Into::into) },
        )
        .await
    }

    // -----------------------------------------------------------------
    // Summary projections
    //
    // The full /v2 responses carry `facts[]` arrays and CDN URLs that
    // dominate payload bytes (~70%) without helping the LLM. The summary
    // mode returns a small projected JSON for typical use, while leaving
    // `summary=false` available when the caller actually wants the raw
    // shape (e.g. to render facts).
    // -----------------------------------------------------------------

    pub async fn get_skills_view(
        &self,
        ids: &[SkillId],
        summary: bool,
    ) -> Result<Value, ServiceError> {
        let map = self.get_skills(ids).await?;
        if summary {
            Ok(Value::Object(
                map.into_iter()
                    .map(|(id, s)| (id.to_string(), summarise_skill(&s)))
                    .collect(),
            ))
        } else {
            Ok(serde_json::to_value(map).unwrap_or(Value::Null))
        }
    }

    pub async fn get_traits_view(
        &self,
        ids: &[TraitId],
        summary: bool,
    ) -> Result<Value, ServiceError> {
        let map = self.get_traits(ids).await?;
        if summary {
            Ok(Value::Object(
                map.into_iter()
                    .map(|(id, t)| (id.to_string(), summarise_trait(&t)))
                    .collect(),
            ))
        } else {
            Ok(serde_json::to_value(map).unwrap_or(Value::Null))
        }
    }

    pub async fn get_specializations_view(
        &self,
        ids: &[SpecializationId],
        summary: bool,
    ) -> Result<Value, ServiceError> {
        let map = self.get_specializations(ids).await?;
        if summary {
            Ok(Value::Object(
                map.into_iter()
                    .map(|(id, s)| (id.to_string(), summarise_specialization(&s)))
                    .collect(),
            ))
        } else {
            Ok(serde_json::to_value(map).unwrap_or(Value::Null))
        }
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
        tab: TabSelector,
    ) -> Result<CharacterBuildSnapshot, ServiceError> {
        let cache_key = format!("character_build:{}:{}", key.fingerprint(), name.as_str());

        let raw_snap: CharacterBuildSnapshot = if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(snap) = serde_json::from_str::<CharacterBuildSnapshot>(&json)
        {
            debug!(character = %name, "character build cache hit");
            snap
        } else {
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
            snap
        };

        // Filter by tab selector.
        let build_tabs = filter_tabs(&raw_snap.build_tabs, tab);
        let equipment_tabs = filter_tabs(&raw_snap.equipment_tabs, tab);

        // Strip cosmetic fields from equipment tabs.
        let equipment_tabs: Vec<Value> = equipment_tabs
            .into_iter()
            .map(strip_equipment_cosmetics)
            .collect();

        // Pre-resolve names on the selected build tabs.
        let resolved_build_tabs = self.resolve_build_tab_names(build_tabs).await;

        Ok(CharacterBuildSnapshot {
            character_name: raw_snap.character_name,
            build_tabs: resolved_build_tabs,
            equipment_tabs,
            fetched_at: raw_snap.fetched_at,
        })
    }

    /// Walk each build tab, collect skill / trait / specialization ids,
    /// resolve them in batch via the cached lookups, and inline `{id, name}`
    /// shapes into the response.
    async fn resolve_build_tab_names(&self, tabs: Vec<Value>) -> Vec<Value> {
        // Collect all ids across all tabs first so a single batched lookup
        // covers everything.
        let mut skill_ids: Vec<SkillId> = Vec::new();
        let mut trait_ids: Vec<TraitId> = Vec::new();
        let mut spec_ids: Vec<SpecializationId> = Vec::new();

        for tab in &tabs {
            collect_build_ids(tab, &mut skill_ids, &mut trait_ids, &mut spec_ids);
        }
        skill_ids.sort_unstable();
        skill_ids.dedup();
        trait_ids.sort_unstable();
        trait_ids.dedup();
        spec_ids.sort_unstable();
        spec_ids.dedup();

        // Parallel fan-out — independent lookups.
        let (skills, traits, specs) = tokio::join!(
            self.get_skills_or_empty(&skill_ids),
            self.get_traits_or_empty(&trait_ids),
            self.get_specializations_or_empty(&spec_ids),
        );

        tabs.into_iter()
            .map(|t| inline_names(t, &skills, &traits, &specs))
            .collect()
    }

    async fn get_skills_or_empty(&self, ids: &[SkillId]) -> BTreeMap<SkillId, Skill> {
        match self.get_skills(ids).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "skill name resolution failed; continuing with raw ids");
                BTreeMap::new()
            }
        }
    }

    async fn get_traits_or_empty(&self, ids: &[TraitId]) -> BTreeMap<TraitId, Trait> {
        match self.get_traits(ids).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "trait name resolution failed; continuing with raw ids");
                BTreeMap::new()
            }
        }
    }

    async fn get_specializations_or_empty(
        &self,
        ids: &[SpecializationId],
    ) -> BTreeMap<SpecializationId, Specialization> {
        match self.get_specializations(ids).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "specialization name resolution failed; continuing with raw ids");
                BTreeMap::new()
            }
        }
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

    // -----------------------------------------------------------------
    // Navigation (Tier 6B): Mumble Link + MapData composition.
    // -----------------------------------------------------------------

    /// Where the player currently is, in their own words.
    pub async fn get_my_location(&self) -> Result<MyLocationSnapshot, ServiceError> {
        let snap = self.mumble.snapshot()?;
        let map_id = snap.context.map_id;
        let map_info = self.get_map_cached(map_id).await.ok();
        let position = (
            f64::from(snap.context.player_x),
            f64::from(snap.context.player_y),
        );
        // Facing in the 2D map plane. Mumble's `f_avatar_front` is a 3D
        // unit vector with Z-up; the X axis aligns with map-east, but the
        // Y axis is map-NORTH (Mumble's Y is up out of the world plane,
        // which corresponds to map-north because GW2's world plane is XY
        // with Y-axis pointing north). Empirically this produces correct
        // bearings; live verification is queued (see runbook).
        let facing = facing_bearing(snap.avatar_front);
        let profession_name = snap
            .identity
            .profession
            .and_then(|p| profession_byte_to_name(p).map(str::to_owned));
        Ok(MyLocationSnapshot {
            character_name: snap.identity.name.clone().unwrap_or_default(),
            profession_name,
            map: map_info.map(map_summary),
            map_id,
            position,
            facing_bearing: facing,
            ui_tick: snap.ui_tick,
            captured_at: self.clock.now(),
        })
    }

    /// Bearing + distance from one point to another. Handles three
    /// `LocationRef` shapes per the MCP schema: literal coords, named
    /// POI on a known map, or "wherever I am right now".
    pub async fn get_directions(
        &self,
        from: LocationRef,
        to: LocationRef,
    ) -> Result<DirectionsResult, ServiceError> {
        let from_pt = self.resolve_location(&from).await?;
        let to_pt = self.resolve_location(&to).await?;
        let bearing16 = bearing(from_pt.coord, to_pt.coord);
        let units = distance_units(from_pt.coord, to_pt.coord);
        let meters = distance_meters(from_pt.coord, to_pt.coord);
        Ok(DirectionsResult {
            from: from_pt,
            to: to_pt,
            bearing: bearing16,
            bearing_label: bearing16.label().to_owned(),
            distance_units: units,
            distance_meters: meters,
        })
    }

    /// Top `limit` POIs near `around`, optionally filtered by kind.
    /// Defaults to the player's current location if `around` is `Here`.
    pub async fn find_nearby(
        &self,
        filter: NearbyFilter,
        around: LocationRef,
        limit: usize,
    ) -> Result<Vec<NearbyResult>, ServiceError> {
        let here = self.resolve_location(&around).await?;
        // We need *some* map id to enumerate POIs. If the caller passed
        // literal coords without a map context, we fall back to the
        // Mumble Link's current map.
        let map_id = match here.map_id {
            Some(m) => m,
            None => self.mumble.snapshot()?.context.map_id,
        };
        let pois = self.list_pois_cached(map_id).await?;
        let mut filtered: Vec<NearbyResult> = pois
            .into_iter()
            .filter(|p| filter.matches(&p.kind))
            .map(|p| {
                let bearing16 = bearing(here.coord, p.coord);
                NearbyResult {
                    distance_units: distance_units(here.coord, p.coord),
                    distance_meters: distance_meters(here.coord, p.coord),
                    bearing: bearing16,
                    bearing_label: bearing16.label().to_owned(),
                    poi: p,
                }
            })
            .collect();
        // Stable sort by distance ascending; ties broken by id for
        // determinism so paginated callers get the same order across
        // requests.
        filtered.sort_by(|a, b| {
            a.distance_units
                .partial_cmp(&b.distance_units)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.poi.id.cmp(&b.poi.id))
        });
        filtered.truncate(limit);
        Ok(filtered)
    }

    /// One-shot description of which way the avatar is currently facing,
    /// plus the closest POI in that quadrant. Surfaces `MumbleError`
    /// directly if the link isn't available.
    pub async fn describe_facing(&self) -> Result<FacingDescription, ServiceError> {
        let snap = self.mumble.snapshot()?;
        let position = (
            f64::from(snap.context.player_x),
            f64::from(snap.context.player_y),
        );
        let facing = facing_bearing(snap.avatar_front);
        let map_id = snap.context.map_id;

        // Best landmark in the same compass direction as we're facing.
        // We don't strictly require an exact match; ±2 wedges (≈45°) is
        // enough to pick a "in front of you" POI.
        let pois = self.list_pois_cached(map_id).await.unwrap_or_default();
        let nearest = pois
            .into_iter()
            .filter(|p| matches!(p.kind.as_str(), "waypoint" | "landmark"))
            .filter_map(|p| {
                let b = bearing(position, p.coord);
                if compass_index(b) == compass_index(facing)
                    || (compass_index_diff(b, facing).unwrap_or(99) <= 2)
                {
                    Some((distance_units(position, p.coord), p, b))
                } else {
                    None
                }
            })
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        let (nearest_landmark, dist_meters) = match nearest {
            Some((_, p, _)) => {
                let m = distance_meters(position, p.coord);
                (Some(p), Some(m))
            }
            None => (None, None),
        };

        Ok(FacingDescription {
            facing_bearing: facing,
            facing_label: facing.label().to_owned(),
            facing_long_name: facing.long_name().to_owned(),
            nearest_landmark,
            nearest_landmark_distance_meters: dist_meters,
            map_id,
            position,
            ui_tick: snap.ui_tick,
        })
    }

    // ----- internal helpers ------------------------------------------------

    async fn resolve_location(&self, loc: &LocationRef) -> Result<ResolvedLocation, ServiceError> {
        match loc {
            LocationRef::Coords { coord, map_id } => Ok(ResolvedLocation {
                coord: *coord,
                map_id: *map_id,
                label: format!("({:.1}, {:.1})", coord.0, coord.1),
            }),
            LocationRef::NamedPoi { map_id, name } => {
                let pois = self.list_pois_cached(*map_id).await?;
                let needle = name.trim().to_ascii_lowercase();
                let hit = pois
                    .into_iter()
                    .find(|p| p.name.trim().to_ascii_lowercase() == needle)
                    .ok_or_else(|| ServiceError::NoSuchPoi {
                        map_id: *map_id,
                        name: name.clone(),
                    })?;
                Ok(ResolvedLocation {
                    coord: hit.coord,
                    map_id: Some(*map_id),
                    label: hit.name,
                })
            }
            LocationRef::Here => {
                let snap = self.mumble.snapshot()?;
                Ok(ResolvedLocation {
                    coord: (
                        f64::from(snap.context.player_x),
                        f64::from(snap.context.player_y),
                    ),
                    map_id: Some(snap.context.map_id),
                    label: "current position".to_owned(),
                })
            }
        }
    }

    async fn get_map_cached(&self, id: MapId) -> Result<MapInfo, ServiceError> {
        let key = format!("map:info:{id}");
        if let Some(json) = self.cache.get(&key).await
            && let Ok(info) = serde_json::from_str::<MapInfo>(&json)
        {
            return Ok(info);
        }
        let info = self.maps.get_map(id).await?;
        if let Ok(json) = serde_json::to_string(&info) {
            self.cache.set(&key, json, STATIC_TTL).await;
        }
        Ok(info)
    }

    async fn list_pois_cached(&self, map_id: MapId) -> Result<Vec<MapPoi>, ServiceError> {
        let key = format!("map:pois:{map_id}");
        if let Some(json) = self.cache.get(&key).await
            && let Ok(pois) = serde_json::from_str::<Vec<MapPoi>>(&json)
        {
            return Ok(pois);
        }
        let pois = self.maps.list_pois(map_id).await?;
        if let Ok(json) = serde_json::to_string(&pois) {
            self.cache.set(&key, json, STATIC_TTL).await;
        }
        Ok(pois)
    }
}

// ---------------------------------------------------------------------------
// Public navigation types — exposed so MCP layer can typed-encode.
// ---------------------------------------------------------------------------

/// One of three ways to identify a navigation target. Encoded as an MCP
/// `oneOf` schema by the stdio adapter.
#[derive(Debug, Clone)]
pub enum LocationRef {
    /// Literal continent-space coordinates. `map_id` is optional; if
    /// supplied, `find_nearby` uses it to scope the POI list. If
    /// omitted, the service falls back to the Mumble Link map id.
    Coords {
        coord: (f64, f64),
        map_id: Option<MapId>,
    },
    /// Named POI on a specific map (case-insensitive match).
    NamedPoi { map_id: MapId, name: String },
    /// "Here" — the player's current Mumble Link position.
    Here,
}

/// Filter applied to POI lists by `find_nearby`. `Any` matches every kind.
#[derive(Debug, Clone, Copy)]
pub enum NearbyFilter {
    Waypoint,
    Poi,
    Vista,
    HeroPoint,
    Task,
    Any,
}

impl NearbyFilter {
    fn matches(self, kind: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Waypoint => kind == "waypoint",
            // `poi` is a meta-bucket: anything that lives under
            // `points_of_interest` in the API and isn't a waypoint or
            // a vista (i.e. landmarks / unlocks).
            Self::Poi => matches!(kind, "landmark" | "unlock"),
            Self::Vista => kind == "vista",
            Self::HeroPoint => kind == "hero_point",
            Self::Task => kind == "task",
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ResolvedLocation {
    pub coord: (f64, f64),
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_id: Option<MapId>,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct MyLocationSnapshot {
    pub character_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profession_name: Option<String>,
    /// Map metadata (name, region, etc.) when the GW2 API lookup
    /// succeeded. None on transient API errors — we return position even
    /// if the metadata failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map: Option<MapSummary>,
    /// Raw map id from the Mumble Link context. Always present.
    pub map_id: MapId,
    pub position: (f64, f64),
    pub facing_bearing: Bearing16,
    pub ui_tick: u32,
    pub captured_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct MapSummary {
    pub id: MapId,
    pub name: String,
    pub region: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map_type: Option<String>,
    pub continent: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DirectionsResult {
    pub from: ResolvedLocation,
    pub to: ResolvedLocation,
    pub bearing: Bearing16,
    pub bearing_label: String,
    pub distance_units: f64,
    pub distance_meters: f64,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct NearbyResult {
    pub poi: MapPoi,
    pub bearing: Bearing16,
    pub bearing_label: String,
    pub distance_units: f64,
    pub distance_meters: f64,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FacingDescription {
    pub facing_bearing: Bearing16,
    pub facing_label: String,
    pub facing_long_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nearest_landmark: Option<MapPoi>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nearest_landmark_distance_meters: Option<f64>,
    pub map_id: MapId,
    pub position: (f64, f64),
    pub ui_tick: u32,
}

fn map_summary(info: MapInfo) -> MapSummary {
    MapSummary {
        id: info.id,
        name: info.name,
        region: info.region_name,
        map_type: info.map_type,
        continent: info.continent_name,
    }
}

/// Convert a Mumble `f_avatar_front` 3D vector to a 16-point compass
/// bearing in the GW2 map plane.
///
/// GW2's world frame is X-east / Y-north / Z-up. Map space inverts the
/// Y axis (Y grows southward) — hence we flip the Y component before
/// running the bearing math, matching the convention `domain::bearing`
/// already encodes for 2D map coords.
fn facing_bearing(front: [f32; 3]) -> Bearing16 {
    let dx = f64::from(front[0]);
    // Mumble's avatar Y axis is map-north; in our Y-down map convention
    // that's a *negative* delta. Pretend the player is at (0,0) and
    // their facing vector lands them at (dx, -dy) one unit ahead.
    let dy_world = f64::from(front[1]);
    bearing((0.0, 0.0), (dx, -dy_world))
}

fn compass_index(b: Bearing16) -> u8 {
    match b {
        // `Same` falls through to 0 — it's only ever passed to
        // `compass_index_diff` which short-circuits before reading it,
        // so this collapse is intentional rather than a missing arm.
        Bearing16::N | Bearing16::Same => 0,
        Bearing16::NNE => 1,
        Bearing16::NE => 2,
        Bearing16::ENE => 3,
        Bearing16::E => 4,
        Bearing16::ESE => 5,
        Bearing16::SE => 6,
        Bearing16::SSE => 7,
        Bearing16::S => 8,
        Bearing16::SSW => 9,
        Bearing16::SW => 10,
        Bearing16::WSW => 11,
        Bearing16::W => 12,
        Bearing16::WNW => 13,
        Bearing16::NW => 14,
        Bearing16::NNW => 15,
    }
}

/// Minimum-wrap distance between two compass indices on the 16-wedge
/// circle. None when either side is `Same`.
fn compass_index_diff(a: Bearing16, b: Bearing16) -> Option<u8> {
    if matches!(a, Bearing16::Same) || matches!(b, Bearing16::Same) {
        return None;
    }
    let i = i16::from(compass_index(a));
    let j = i16::from(compass_index(b));
    let raw = (i - j).rem_euclid(16);
    Some(u8::try_from(raw.min(16 - raw)).unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Profession byte → name map. Stable for the life of the game; carrying it
// in code rather than going through GW2 /v2/professions saves an HTTP round
// trip on every decode_build_code call.
// ---------------------------------------------------------------------------

fn profession_byte_to_name(byte: u8) -> Option<&'static str> {
    match byte {
        1 => Some("Guardian"),
        2 => Some("Warrior"),
        3 => Some("Engineer"),
        4 => Some("Ranger"),
        5 => Some("Thief"),
        6 => Some("Elementalist"),
        7 => Some("Mesmer"),
        8 => Some("Necromancer"),
        9 => Some("Revenant"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Summary projections
// ---------------------------------------------------------------------------

/// Pull a key from a Skill/Trait/Specialization's `extra` map into a json
/// value, omitting `null` and empty-string outputs to keep payloads tight.
fn extract<T: AsRef<str>>(extra: &BTreeMap<String, Value>, k: T) -> Option<Value> {
    let v = extra.get(k.as_ref())?;
    match v {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        _ => Some(v.clone()),
    }
}

fn summarise_skill(s: &Skill) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_owned(), json!(s.id.get()));
    obj.insert("name".to_owned(), json!(s.name));
    for k in [
        "description",
        "type",
        "slot",
        "professions",
        "weapon_type",
        "chat_link",
    ] {
        if let Some(v) = extract(&s.extra, k) {
            obj.insert(k.to_owned(), v);
        }
    }
    Value::Object(obj)
}

fn summarise_trait(t: &Trait) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_owned(), json!(t.id.get()));
    obj.insert("name".to_owned(), json!(t.name));
    for k in ["description", "specialization", "tier", "slot"] {
        if let Some(v) = extract(&t.extra, k) {
            obj.insert(k.to_owned(), v);
        }
    }
    Value::Object(obj)
}

fn summarise_specialization(s: &Specialization) -> Value {
    let mut obj = Map::new();
    obj.insert("id".to_owned(), json!(s.id.get()));
    obj.insert("name".to_owned(), json!(s.name));
    for k in ["profession", "elite", "minor_traits", "major_traits"] {
        if let Some(v) = extract(&s.extra, k) {
            obj.insert(k.to_owned(), v);
        }
    }
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Tab filtering + equipment cosmetic-stripping for character builds
// ---------------------------------------------------------------------------

fn filter_tabs(tabs: &[Value], sel: TabSelector) -> Vec<Value> {
    match sel {
        TabSelector::All => tabs.to_vec(),
        TabSelector::Active => tabs
            .iter()
            .find(|t| {
                t.get("is_active").and_then(Value::as_bool).unwrap_or(false)
                    || t.get("is_active_equipment_template")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            })
            .cloned()
            .into_iter()
            .collect(),
        TabSelector::Index(n) => tabs
            .iter()
            .find(|t| t.get("tab").and_then(Value::as_u64) == Some(u64::from(n)))
            .cloned()
            .into_iter()
            .collect(),
    }
}

/// Drop cosmetic fields from each equipment piece on this tab (`dyes`,
/// `bound_to`, `binding`, `location`). LLMs reasoning about builds don't need
/// them; they're pure visual / inventory-state metadata.
fn strip_equipment_cosmetics(mut tab: Value) -> Value {
    let Some(eq_arr) = tab.get_mut("equipment").and_then(Value::as_array_mut) else {
        return tab;
    };
    for piece in eq_arr.iter_mut() {
        if let Some(obj) = piece.as_object_mut() {
            for k in ["dyes", "bound_to", "binding", "location"] {
                obj.remove(k);
            }
        }
    }
    tab
}

// ---------------------------------------------------------------------------
// Build-tab id collection + name inlining
// ---------------------------------------------------------------------------

fn collect_build_ids(
    tab: &Value,
    skills: &mut Vec<SkillId>,
    traits: &mut Vec<TraitId>,
    specs: &mut Vec<SpecializationId>,
) {
    let Some(build) = tab.get("build") else {
        return;
    };

    // skills.heal, skills.utilities[], skills.elite + aquatic_skills.*
    for skill_block in ["skills", "aquatic_skills"] {
        let Some(block) = build.get(skill_block) else {
            continue;
        };
        for k in ["heal", "elite"] {
            push_skill(block.get(k), skills);
        }
        if let Some(arr) = block.get("utilities").and_then(Value::as_array) {
            for v in arr {
                push_skill(Some(v), skills);
            }
        }
    }

    // specializations[].id, specializations[].traits[]
    if let Some(arr) = build.get("specializations").and_then(Value::as_array) {
        for s in arr {
            if let Some(id) = s
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|n| SpecializationId::new(i64::try_from(n).ok()?).ok())
            {
                specs.push(id);
            }
            if let Some(t_arr) = s.get("traits").and_then(Value::as_array) {
                for t in t_arr {
                    if let Some(id) = t
                        .as_u64()
                        .and_then(|n| TraitId::new(i64::try_from(n).ok()?).ok())
                    {
                        traits.push(id);
                    }
                }
            }
        }
    }
}

fn push_skill(v: Option<&Value>, out: &mut Vec<SkillId>) {
    if let Some(id) = v
        .and_then(Value::as_u64)
        .and_then(|n| SkillId::new(i64::try_from(n).ok()?).ok())
    {
        out.push(id);
    }
}

fn inline_names(
    mut tab: Value,
    skills: &BTreeMap<SkillId, Skill>,
    traits: &BTreeMap<TraitId, Trait>,
    specs: &BTreeMap<SpecializationId, Specialization>,
) -> Value {
    let Some(build) = tab.get_mut("build").and_then(Value::as_object_mut) else {
        return tab;
    };

    for skill_block in ["skills", "aquatic_skills"] {
        if let Some(block) = build.get_mut(skill_block).and_then(Value::as_object_mut) {
            for k in ["heal", "elite"] {
                if let Some(v) = block.get_mut(k) {
                    *v = inline_skill(v, skills);
                }
            }
            if let Some(arr) = block.get_mut("utilities").and_then(Value::as_array_mut) {
                for v in arr.iter_mut() {
                    *v = inline_skill(v, skills);
                }
            }
        }
    }

    if let Some(arr) = build
        .get_mut("specializations")
        .and_then(Value::as_array_mut)
    {
        for s in arr.iter_mut() {
            let spec_id = s
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|n| SpecializationId::new(i64::try_from(n).ok()?).ok());
            if let (Some(obj), Some(id)) = (s.as_object_mut(), spec_id)
                && let Some(spec) = specs.get(&id)
            {
                obj.insert("name".to_owned(), json!(spec.name));
            }
            if let Some(t_arr) = s.get_mut("traits").and_then(Value::as_array_mut) {
                for t in t_arr.iter_mut() {
                    *t = inline_trait(t, traits);
                }
            }
        }
    }

    tab
}

fn inline_skill(v: &Value, skills: &BTreeMap<SkillId, Skill>) -> Value {
    let Some(id) = v
        .as_u64()
        .and_then(|n| SkillId::new(i64::try_from(n).ok()?).ok())
    else {
        return v.clone();
    };
    let name = skills.get(&id).map(|s| s.name.clone()).unwrap_or_default();
    json!({ "id": id.get(), "name": name })
}

fn inline_trait(v: &Value, traits: &BTreeMap<TraitId, Trait>) -> Value {
    let Some(id) = v
        .as_u64()
        .and_then(|n| TraitId::new(i64::try_from(n).ok()?).ok())
    else {
        return v.clone();
    };
    let name = traits.get(&id).map(|t| t.name.clone()).unwrap_or_default();
    json!({ "id": id.get(), "name": name })
}

// ---------------------------------------------------------------------------
// Cache key helpers — module-local so tests can pin them
// ---------------------------------------------------------------------------

fn wallet_cache_key(key: &ApiKey) -> String {
    format!("wallet:{}", key.fingerprint())
}

fn account_cache_key(key: &ApiKey) -> String {
    format!("account:{}", key.fingerprint())
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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

    #[test]
    fn profession_byte_map_is_complete() {
        for byte in 1u8..=9 {
            assert!(profession_byte_to_name(byte).is_some());
        }
        assert!(profession_byte_to_name(0).is_none());
        assert!(profession_byte_to_name(10).is_none());
    }
}
