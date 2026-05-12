//! Background indexing pipeline (Tier 6C).
//!
//! Orchestrates the bulk-fetch + upsert cycle that turns the GW2 reference
//! corpus into a queryable on-disk index. Sits at the same layer as
//! [`crate::service::Service`] — it composes the [`Gw2Api`] and
//! [`SearchIndex`] ports without ever depending on a concrete adapter.
//!
//! ## Lifecycle
//!
//! 1. `ensure_fresh` is the entry point: it cheaply asks `/v2/build` for the
//!    current game build number. If the index already carries that number,
//!    nothing is done. Otherwise a full refresh is dispatched.
//! 2. `run_full_refresh` walks each entity kind (skills, traits, specs,
//!    achievements; items only if `include_items` is set), enumerates ids
//!    via `/v2/<kind>` (no `?ids=`), then fetches details in 200-id chunks
//!    with a small inter-batch sleep to stay under the 600 req/min budget.
//!    Per-batch failures log + continue rather than abort the whole run —
//!    a partial index is better than no index.
//! 3. After every kind succeeds, `set_build_number(current)` stamps the
//!    index. A crash mid-flight leaves the stamp from the previous build,
//!    so the next startup retries from scratch.
//!
//! ## Why not a worker pool
//!
//! GW2's per-key rate limit is 600 req/min. Even with full pipelining we
//! hit that in seconds, so concurrency would just block on the bucket. A
//! single-task sequential walk is simpler, easier to reason about, and
//! puts no load on the `SQLite` single-writer mutex.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tracing::{info, warn};

use crate::ports::{Gw2Api, Gw2ApiError, SearchError, SearchIndex};

/// Per-batch sleep to keep us comfortably under the 600 req/min budget when
/// many batches run back-to-back. With a 200-id chunk, ~5 batches/sec is
/// fine (the limit is 10 req/sec) but politeness costs us nothing.
const INTER_BATCH_SLEEP: Duration = Duration::from_millis(100);

/// GW2's per-request id cap. Mirrors the constant in `HttpGw2Api::fetch_by_ids`
/// — kept duplicated here so the pipeline can chunk before crossing the
/// adapter boundary, which keeps each per-chunk error / log line small.
const ID_CHUNK_SIZE: usize = 200;

#[derive(Debug, Error)]
pub enum IndexingError {
    #[error("{0}")]
    Gw2(#[from] Gw2ApiError),

    #[error("{0}")]
    Search(#[from] SearchError),
}

/// Knobs that callers (the binary, integration tests) toggle when they
/// build a pipeline. Defaults are conservative: items are *not* indexed by
/// default because they cost ~5 minutes + ~50 MB on disk.
#[derive(Debug, Clone, Copy, Default)]
pub struct IndexingOpts {
    /// Whether to include `/v2/items` in the background refresh. Heavy:
    /// ~85k entries, ~5 minutes of API calls, ~50 MB on disk.
    pub include_items: bool,
    /// If `true`, ignore the cached build number and re-index from scratch.
    pub force_rebuild: bool,
}

/// Composes the GW2 API + the search index into the bulk-refresh loop.
///
/// `Clone` is cheap (everything inside is already `Arc`-wrapped); the
/// pipeline can be stamped into both the foreground service and a
/// background refresh task.
#[derive(Clone)]
pub struct IndexingPipeline {
    gw2: Arc<dyn Gw2Api>,
    idx: Arc<dyn SearchIndex>,
    opts: IndexingOpts,
}

impl IndexingPipeline {
    pub fn new(gw2: Arc<dyn Gw2Api>, idx: Arc<dyn SearchIndex>, opts: IndexingOpts) -> Self {
        Self { gw2, idx, opts }
    }

    /// Decide whether a refresh is needed and run it inline.
    ///
    /// Returns `Ok(true)` if a refresh ran, `Ok(false)` if the index was
    /// already fresh. Errors bubble up — callers usually log + ignore so
    /// the server still starts even when the indexer can't reach upstream.
    pub async fn ensure_fresh(&self) -> Result<bool, IndexingError> {
        let current = self.gw2.fetch_build().await?;
        if !self.opts.force_rebuild
            && let Some(cached) = self.idx.build_number().await?
            && cached == current
        {
            info!(build = current, "search index is fresh; nothing to do");
            return Ok(false);
        }
        info!(
            build = current,
            force = self.opts.force_rebuild,
            "search index refresh starting"
        );
        self.run_full_refresh(current).await?;
        info!(build = current, "search index refresh complete");
        Ok(true)
    }

    /// Spawn a fire-and-forget tokio task that runs `ensure_fresh`. Used
    /// from `main.rs` so the binary can start serving MCP traffic
    /// immediately — searches return `NotIndexed` until the task lands its
    /// first batch, which is a clean, typed error the LLM can interpret.
    pub fn spawn_background(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            match self.ensure_fresh().await {
                Ok(true) => info!("background indexer finished (refreshed)"),
                Ok(false) => info!("background indexer finished (already fresh)"),
                Err(e) => {
                    warn!(error = ?e, "background indexer failed; search will return NotIndexed");
                }
            }
        })
    }

    /// Walk every kind. Per-kind failures are logged and skipped — a
    /// partial index is more useful than no index.
    async fn run_full_refresh(&self, build: u32) -> Result<(), IndexingError> {
        if let Err(e) = self.refresh_skills(build).await {
            warn!(error = ?e, "skills refresh failed; partial index");
        }
        if let Err(e) = self.refresh_traits(build).await {
            warn!(error = ?e, "traits refresh failed; partial index");
        }
        if let Err(e) = self.refresh_specializations(build).await {
            warn!(error = ?e, "specializations refresh failed; partial index");
        }
        if let Err(e) = self.refresh_achievements(build).await {
            warn!(error = ?e, "achievements refresh failed; partial index");
        }
        if self.opts.include_items
            && let Err(e) = self.refresh_items(build).await
        {
            warn!(error = ?e, "items refresh failed; partial index");
        }
        // Stamp at the end. If a kind partially failed we still stamp —
        // the next start sees the build number, finds no entries for the
        // missing kind, and the `NotIndexed` error guides the user toward
        // the typed `get_*` tools or `--rebuild-index`.
        self.idx.set_build_number(build).await?;
        Ok(())
    }

    async fn refresh_skills(&self, build: u32) -> Result<(), IndexingError> {
        let ids = self.gw2.fetch_all_skill_ids().await?;
        info!(kind = "skills", total = ids.len(), "indexing");
        for chunk in ids.chunks(ID_CHUNK_SIZE) {
            match self.gw2.fetch_skills(chunk).await {
                Ok(map) => {
                    let batch: Vec<_> = map.into_values().collect();
                    if let Err(e) = self.idx.upsert_skills(&batch, build).await {
                        warn!(error = ?e, "skills upsert failed for batch; continuing");
                    }
                }
                Err(e) => warn!(error = ?e, "skills fetch failed for batch; continuing"),
            }
            tokio::time::sleep(INTER_BATCH_SLEEP).await;
        }
        Ok(())
    }

    async fn refresh_traits(&self, build: u32) -> Result<(), IndexingError> {
        let ids = self.gw2.fetch_all_trait_ids().await?;
        info!(kind = "traits", total = ids.len(), "indexing");
        for chunk in ids.chunks(ID_CHUNK_SIZE) {
            match self.gw2.fetch_traits(chunk).await {
                Ok(map) => {
                    let batch: Vec<_> = map.into_values().collect();
                    if let Err(e) = self.idx.upsert_traits(&batch, build).await {
                        warn!(error = ?e, "traits upsert failed for batch; continuing");
                    }
                }
                Err(e) => warn!(error = ?e, "traits fetch failed for batch; continuing"),
            }
            tokio::time::sleep(INTER_BATCH_SLEEP).await;
        }
        Ok(())
    }

    async fn refresh_specializations(&self, build: u32) -> Result<(), IndexingError> {
        let ids = self.gw2.fetch_all_specialization_ids().await?;
        info!(kind = "specializations", total = ids.len(), "indexing");
        for chunk in ids.chunks(ID_CHUNK_SIZE) {
            match self.gw2.fetch_specializations(chunk).await {
                Ok(map) => {
                    let batch: Vec<_> = map.into_values().collect();
                    if let Err(e) = self.idx.upsert_specializations(&batch, build).await {
                        warn!(error = ?e, "specs upsert failed for batch; continuing");
                    }
                }
                Err(e) => warn!(error = ?e, "specs fetch failed for batch; continuing"),
            }
            tokio::time::sleep(INTER_BATCH_SLEEP).await;
        }
        Ok(())
    }

    async fn refresh_items(&self, build: u32) -> Result<(), IndexingError> {
        let ids = self.gw2.fetch_all_item_ids().await?;
        info!(
            kind = "items",
            total = ids.len(),
            "indexing (this is the slow one)"
        );
        for chunk in ids.chunks(ID_CHUNK_SIZE) {
            match self.gw2.fetch_items(chunk).await {
                Ok(map) => {
                    let batch: Vec<_> = map.into_values().collect();
                    if let Err(e) = self.idx.upsert_items(&batch, build).await {
                        warn!(error = ?e, "items upsert failed for batch; continuing");
                    }
                }
                Err(e) => warn!(error = ?e, "items fetch failed for batch; continuing"),
            }
            tokio::time::sleep(INTER_BATCH_SLEEP).await;
        }
        Ok(())
    }

    async fn refresh_achievements(&self, build: u32) -> Result<(), IndexingError> {
        let ids = self.gw2.fetch_all_achievement_ids().await?;
        info!(kind = "achievements", total = ids.len(), "indexing");
        for chunk in ids.chunks(ID_CHUNK_SIZE) {
            match self.gw2.fetch_achievements(chunk).await {
                Ok(map) => {
                    let batch: Vec<_> = map.into_values().collect();
                    if let Err(e) = self.idx.upsert_achievements(&batch, build).await {
                        warn!(error = ?e, "achievements upsert failed for batch; continuing");
                    }
                }
                Err(e) => warn!(error = ?e, "achievements fetch failed for batch; continuing"),
            }
            tokio::time::sleep(INTER_BATCH_SLEEP).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::SqliteSearchIndex;
    use crate::domain::{
        Achievement, AchievementId, ApiKey, CharacterName, Currency, CurrencyId, Item, ItemId,
        Skill, SkillId, Specialization, SpecializationId, Trait, TraitId, WalletEntry,
    };
    use crate::ports::{Gw2Api, SearchIndex, SkillSearchFilter};
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Minimal canned-data `Gw2Api` fake. Lives in this module (not in
    /// `tests/common`) so unit tests inside the crate can use it without
    /// pulling the whole tests/common harness into scope.
    struct FakeApi {
        build: Mutex<u32>,
        skills: Mutex<BTreeMap<SkillId, Skill>>,
        traits: Mutex<BTreeMap<TraitId, Trait>>,
        specs: Mutex<BTreeMap<SpecializationId, Specialization>>,
        items: Mutex<BTreeMap<ItemId, Item>>,
        achievements: Mutex<BTreeMap<AchievementId, Achievement>>,
        // Counters
        skills_fetch_calls: Mutex<usize>,
        items_fetch_calls: Mutex<usize>,
    }

    impl FakeApi {
        fn new(build: u32) -> Arc<Self> {
            Arc::new(Self {
                build: Mutex::new(build),
                skills: Mutex::new(BTreeMap::new()),
                traits: Mutex::new(BTreeMap::new()),
                specs: Mutex::new(BTreeMap::new()),
                items: Mutex::new(BTreeMap::new()),
                achievements: Mutex::new(BTreeMap::new()),
                skills_fetch_calls: Mutex::new(0),
                items_fetch_calls: Mutex::new(0),
            })
        }
    }

    #[async_trait]
    impl Gw2Api for FakeApi {
        async fn fetch_wallet(&self, _: &ApiKey) -> Result<Vec<WalletEntry>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_currency_ids(&self) -> Result<Vec<CurrencyId>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_currencies(
            &self,
            _: &[CurrencyId],
        ) -> Result<BTreeMap<CurrencyId, Currency>, Gw2ApiError> {
            Ok(BTreeMap::new())
        }
        async fn fetch_skills(
            &self,
            ids: &[SkillId],
        ) -> Result<BTreeMap<SkillId, Skill>, Gw2ApiError> {
            *self.skills_fetch_calls.lock().unwrap() += 1;
            let store = self.skills.lock().unwrap();
            Ok(ids
                .iter()
                .filter_map(|id| store.get(id).map(|s| (*id, s.clone())))
                .collect())
        }
        async fn fetch_traits(
            &self,
            ids: &[TraitId],
        ) -> Result<BTreeMap<TraitId, Trait>, Gw2ApiError> {
            let store = self.traits.lock().unwrap();
            Ok(ids
                .iter()
                .filter_map(|id| store.get(id).map(|s| (*id, s.clone())))
                .collect())
        }
        async fn fetch_specializations(
            &self,
            ids: &[SpecializationId],
        ) -> Result<BTreeMap<SpecializationId, Specialization>, Gw2ApiError> {
            let store = self.specs.lock().unwrap();
            Ok(ids
                .iter()
                .filter_map(|id| store.get(id).map(|s| (*id, s.clone())))
                .collect())
        }
        async fn fetch_items(&self, ids: &[ItemId]) -> Result<BTreeMap<ItemId, Item>, Gw2ApiError> {
            *self.items_fetch_calls.lock().unwrap() += 1;
            let store = self.items.lock().unwrap();
            Ok(ids
                .iter()
                .filter_map(|id| store.get(id).map(|s| (*id, s.clone())))
                .collect())
        }
        async fn fetch_achievements(
            &self,
            ids: &[AchievementId],
        ) -> Result<BTreeMap<AchievementId, Achievement>, Gw2ApiError> {
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
            Ok(*self.build.lock().unwrap())
        }
        async fn fetch_buildtabs(
            &self,
            _: &ApiKey,
            _: &CharacterName,
        ) -> Result<Vec<serde_json::Value>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_equipmenttabs(
            &self,
            _: &ApiKey,
            _: &CharacterName,
        ) -> Result<Vec<serde_json::Value>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_account(&self, _: &ApiKey) -> Result<crate::domain::Account, Gw2ApiError> {
            Err(Gw2ApiError::Decode(
                "FakeApi: not used in indexing tests".into(),
            ))
        }
        async fn fetch_characters_list(&self, _: &ApiKey) -> Result<Vec<String>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_account_achievements(
            &self,
            _: &ApiKey,
        ) -> Result<Vec<crate::domain::AccountAchievement>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_account_masteries(
            &self,
            _: &ApiKey,
        ) -> Result<Vec<crate::domain::AccountMastery>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_account_mastery_points(
            &self,
            _: &ApiKey,
        ) -> Result<crate::domain::AccountMasteryPoints, Gw2ApiError> {
            Ok(crate::domain::AccountMasteryPoints {
                totals: Vec::new(),
                unlocked: Vec::new(),
            })
        }
        async fn fetch_account_bank(
            &self,
            _: &ApiKey,
        ) -> Result<Vec<crate::domain::InventorySlot>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_all_mastery_ids(
            &self,
        ) -> Result<Vec<crate::domain::MasteryId>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_masteries(
            &self,
            _: &[crate::domain::MasteryId],
        ) -> Result<BTreeMap<crate::domain::MasteryId, crate::domain::Mastery>, Gw2ApiError>
        {
            Ok(BTreeMap::new())
        }
        async fn fetch_account_raids(&self, _: &ApiKey) -> Result<Vec<String>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_all_raid_ids(&self) -> Result<Vec<String>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_raids(
            &self,
            _: &[String],
        ) -> Result<BTreeMap<String, crate::domain::Raid>, Gw2ApiError> {
            Ok(BTreeMap::new())
        }
        async fn fetch_account_dungeons(&self, _: &ApiKey) -> Result<Vec<String>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_all_dungeon_ids(&self) -> Result<Vec<String>, Gw2ApiError> {
            Ok(Vec::new())
        }
        async fn fetch_dungeons(
            &self,
            _: &[String],
        ) -> Result<BTreeMap<String, crate::domain::Dungeon>, Gw2ApiError> {
            Ok(BTreeMap::new())
        }
        async fn fetch_wizards_vault_daily(
            &self,
            _: &ApiKey,
        ) -> Result<crate::domain::WizardsVaultTrack, Gw2ApiError> {
            Err(Gw2ApiError::Decode(
                "FakeApi: not used in indexing tests".into(),
            ))
        }
        async fn fetch_wizards_vault_weekly(
            &self,
            _: &ApiKey,
        ) -> Result<crate::domain::WizardsVaultTrack, Gw2ApiError> {
            Err(Gw2ApiError::Decode(
                "FakeApi: not used in indexing tests".into(),
            ))
        }
        async fn fetch_wizards_vault_special(
            &self,
            _: &ApiKey,
        ) -> Result<crate::domain::WizardsVaultTrack, Gw2ApiError> {
            Err(Gw2ApiError::Decode(
                "FakeApi: not used in indexing tests".into(),
            ))
        }
        async fn fetch_regions_on_floor(
            &self,
            _: u32,
            _: u32,
        ) -> Result<BTreeMap<u32, crate::domain::Region>, Gw2ApiError> {
            Ok(BTreeMap::new())
        }
    }

    fn skill(id: u32, name: &str) -> Skill {
        let mut extra = BTreeMap::new();
        extra.insert("description".into(), json!(format!("d {name}")));
        extra.insert("professions".into(), json!(["Mesmer"]));
        Skill {
            id: SkillId::new(i64::from(id)).unwrap(),
            name: name.to_owned(),
            extra,
        }
    }

    #[tokio::test]
    async fn ensure_fresh_skips_when_build_matches() {
        let api = FakeApi::new(99);
        api.skills
            .lock()
            .unwrap()
            .insert(SkillId::new(1).unwrap(), skill(1, "S"));
        let idx: Arc<dyn SearchIndex> =
            Arc::new(SqliteSearchIndex::open_in_memory().await.unwrap());
        idx.set_build_number(99).await.unwrap();
        let pipeline = IndexingPipeline::new(api.clone(), idx, IndexingOpts::default());
        let refreshed = pipeline.ensure_fresh().await.unwrap();
        assert!(!refreshed, "should skip when build matches");
        assert_eq!(*api.skills_fetch_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn ensure_fresh_runs_when_build_differs_and_skips_items_by_default() {
        let api = FakeApi::new(100);
        api.skills
            .lock()
            .unwrap()
            .insert(SkillId::new(1).unwrap(), skill(1, "Mind Wrack"));
        api.items.lock().unwrap().insert(ItemId::new(1).unwrap(), {
            let mut e = BTreeMap::new();
            e.insert("type".into(), json!("Weapon"));
            Item {
                id: ItemId::new(1).unwrap(),
                name: "Greatsword".to_owned(),
                extra: e,
            }
        });
        let idx_concrete = SqliteSearchIndex::open_in_memory().await.unwrap();
        let idx: Arc<dyn SearchIndex> = Arc::new(idx_concrete);
        let pipeline = IndexingPipeline::new(api.clone(), idx.clone(), IndexingOpts::default());

        let refreshed = pipeline.ensure_fresh().await.unwrap();
        assert!(refreshed);
        assert_eq!(idx.build_number().await.unwrap(), Some(100));
        // Items skipped → no fetch_items calls.
        assert_eq!(*api.items_fetch_calls.lock().unwrap(), 0);

        let res = idx
            .search_skills("mind", 5, SkillSearchFilter::default())
            .await
            .unwrap();
        assert_eq!(res.len(), 1);
    }

    #[tokio::test]
    async fn force_rebuild_runs_even_when_build_matches() {
        let api = FakeApi::new(7);
        api.skills
            .lock()
            .unwrap()
            .insert(SkillId::new(1).unwrap(), skill(1, "S"));
        let idx: Arc<dyn SearchIndex> =
            Arc::new(SqliteSearchIndex::open_in_memory().await.unwrap());
        idx.set_build_number(7).await.unwrap();
        let pipeline = IndexingPipeline::new(
            api.clone(),
            idx,
            IndexingOpts {
                force_rebuild: true,
                ..Default::default()
            },
        );
        let refreshed = pipeline.ensure_fresh().await.unwrap();
        assert!(refreshed);
        assert!(*api.skills_fetch_calls.lock().unwrap() >= 1);
    }

    #[tokio::test]
    async fn ensure_fresh_with_include_items_indexes_items() {
        let api = FakeApi::new(50);
        api.items.lock().unwrap().insert(ItemId::new(1).unwrap(), {
            let mut e = BTreeMap::new();
            e.insert("type".into(), json!("Weapon"));
            e.insert("rarity".into(), json!("Exotic"));
            Item {
                id: ItemId::new(1).unwrap(),
                name: "Berserker's Greatsword".to_owned(),
                extra: e,
            }
        });
        let idx_concrete = SqliteSearchIndex::open_in_memory().await.unwrap();
        let idx: Arc<dyn SearchIndex> = Arc::new(idx_concrete);
        let pipeline = IndexingPipeline::new(
            api.clone(),
            idx.clone(),
            IndexingOpts {
                include_items: true,
                ..Default::default()
            },
        );
        pipeline.ensure_fresh().await.unwrap();
        assert!(*api.items_fetch_calls.lock().unwrap() >= 1);
    }
}
