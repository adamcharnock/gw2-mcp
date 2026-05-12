//! Account-scoped endpoints — wallet, account, achievements, masteries,
//! raid/dungeon clears, Wizard's Vault objectives. All cached on
//! `WALLET_TTL` (5 min) except `get_dailies` which uses `DAILIES_TTL`
//! (1 hour). Cache keys derive from `key.fingerprint()` — the raw API
//! key never touches a cache key.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::{DAILIES_TTL, STATIC_TTL, Service, ServiceError, WALLET_TTL};
use crate::domain::{
    Account, AccountAchievement, AccountMastery, Achievement, AchievementId, ApiKey, Currency,
    CurrencyId, Dungeon, Raid, WalletEntry, WalletInfo, WizardsVaultTrack, next_daily_reset,
    next_raid_reset, title_case,
};

/// Which Wizard's Vault track to fetch — `Daily`, `Weekly`, or
/// `Special` (limited-time / seasonal). Defaults to `Daily` because
/// "what should I do?" almost always means right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DailiesWhich {
    #[default]
    Daily,
    Weekly,
    Special,
}

impl Service {
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

    /// Invalidate every cached entry tied to this API key's
    /// fingerprint. Use case: the user just bought an expansion (or
    /// completed a Wizard's Vault objective, or claimed a daily
    /// reward) and wants the next call to refetch from
    /// `/v2/account` instead of returning the cached snapshot.
    ///
    /// Returns the list of cache keys that were cleared so the
    /// caller can echo it back for transparency. Cache keys that
    /// weren't populated are still listed — `Cache::delete` is a
    /// no-op for absent keys, and "I cleared X" is more useful than
    /// "I cleared whichever subset of X had been populated".
    pub async fn refresh_account_cache(&self, key: &ApiKey) -> RefreshAccountCacheResult {
        let fingerprint = key.fingerprint().clone();
        let keys = [
            format!("account:{fingerprint}"),
            format!("wallet:{fingerprint}"),
            format!("characters:{fingerprint}"),
            format!("account_achievements:{fingerprint}"),
            format!("account_masteries:{fingerprint}"),
            format!("account_raids:{fingerprint}"),
            format!("account_dungeons:{fingerprint}"),
            format!("wizardsvault:daily:{fingerprint}"),
            format!("wizardsvault:weekly:{fingerprint}"),
            format!("wizardsvault:special:{fingerprint}"),
        ];
        for k in &keys {
            self.cache.delete(k).await;
        }
        debug!(fingerprint = %fingerprint, cleared = keys.len(), "account cache cleared");
        RefreshAccountCacheResult {
            cleared_at: self.clock.now(),
            cleared_count: keys.len(),
            scopes: [
                "account",
                "wallet",
                "characters",
                "achievements",
                "masteries",
                "raids",
                "dungeons",
                "wizards_vault",
            ]
            .iter()
            .map(|s| String::from(*s))
            .collect(),
        }
    }

    /// Fetch character names only (`/v2/characters`). Cached `WALLET_TTL`.
    ///
    /// Wraps the bare API array in a [`CharacterList`] so MCP's
    /// `structuredContent` (object-only) schema accepts it.
    pub async fn list_characters(&self, key: &ApiKey) -> Result<CharacterList, ServiceError> {
        let cache_key = format!("characters:{}", key.fingerprint());
        let names: Vec<String> = if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str::<Vec<String>>(&json)
        {
            v
        } else {
            let v = self.gw2.fetch_characters_list(key).await?;
            if let Ok(json) = serde_json::to_string(&v) {
                self.cache.set(&cache_key, json, WALLET_TTL).await;
            }
            v
        };
        Ok(CharacterList {
            total: names.len(),
            characters: names,
        })
    }

    /// Fetch the per-account achievement progress list, enriched with
    /// achievement names + descriptions so the LLM doesn't have to
    /// follow up with `get_achievements` to know what `id=1234` is.
    /// Cached `WALLET_TTL`.
    ///
    /// When `summary` is true (the LLM-friendly default), drops entries
    /// where the user is fully done (`done==true` or `current==max`) and
    /// entries where they haven't started (`current==0` or `current` is
    /// missing). What's left is the "what am I working on?" set — usually
    /// 100-300 entries instead of 2000-3000.
    ///
    /// Metadata lookups are best-effort: a transient `/v2/achievements`
    /// failure leaves `name`/`description` as `None` rather than failing
    /// the whole tool call.
    pub async fn get_account_achievements(
        &self,
        key: &ApiKey,
        summary: bool,
    ) -> Result<AccountAchievementsSnapshot, ServiceError> {
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
        let filtered: Vec<AccountAchievement> = if summary {
            raw.into_iter()
                .filter(|a| !a.is_completed() && !a.is_not_started())
                .collect()
        } else {
            raw
        };
        let metadata = self
            .achievement_metadata_for_ids(filtered.iter().map(|a| a.id))
            .await;
        let achievements: Vec<AccountAchievementEntry> = filtered
            .into_iter()
            .map(|progress| {
                let (name, description) = lookup_name_description(&metadata, progress.id);
                AccountAchievementEntry {
                    progress,
                    name,
                    description,
                }
            })
            .collect();
        Ok(AccountAchievementsSnapshot {
            total: achievements.len(),
            summary,
            achievements,
            fetched_at: self.clock.now(),
        })
    }

    /// Fetch unlocked-mastery progress per track, enriched with track
    /// name + region + the name of the current level. Account state
    /// cached `WALLET_TTL`; the mastery metadata table itself is fetched
    /// once and cached under `STATIC_TTL`.
    pub async fn get_account_masteries(
        &self,
        key: &ApiKey,
    ) -> Result<AccountMasteriesSnapshot, ServiceError> {
        let cache_key = format!("account_masteries:{}", key.fingerprint());
        let progress: Vec<AccountMastery> = if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str::<Vec<AccountMastery>>(&json)
        {
            v
        } else {
            let v = self.gw2.fetch_account_masteries(key).await?;
            if let Ok(json) = serde_json::to_string(&v) {
                self.cache.set(&cache_key, json, WALLET_TTL).await;
            }
            v
        };
        let metadata = self.mastery_metadata_table().await;
        let total_points_earned = total_mastery_points_earned(&progress, &metadata);
        let masteries: Vec<AccountMasteryEntry> = progress
            .into_iter()
            .map(|p| enrich_mastery(p, &metadata))
            .collect();
        Ok(AccountMasteriesSnapshot {
            total: masteries.len(),
            total_points_earned,
            masteries,
            fetched_at: self.clock.now(),
        })
    }

    /// Fetch + cache the full `/v2/masteries` table. Mastery tracks are
    /// effectively static — the table changes only when `ArenaNet` ships a
    /// new expansion — so we cache the entire `BTreeMap<MasteryId, Mastery>`
    /// under one `STATIC_TTL` key (`masteries:all`). Empty `BTreeMap` on
    /// any failure so the caller falls back to ids-only.
    pub(super) async fn mastery_metadata_table(
        &self,
    ) -> BTreeMap<crate::domain::MasteryId, crate::domain::Mastery> {
        use crate::domain::{Mastery, MasteryId};
        const KEY: &str = "masteries:all";
        if let Some(json) = self.cache.get(KEY).await
            && let Ok(map) = serde_json::from_str::<BTreeMap<MasteryId, Mastery>>(&json)
        {
            return map;
        }
        let ids = match self.gw2.fetch_all_mastery_ids().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = ?e, "failed to enumerate mastery ids; mastery metadata unavailable");
                return BTreeMap::new();
            }
        };
        if ids.is_empty() {
            return BTreeMap::new();
        }
        let map = match self.gw2.fetch_masteries(&ids).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "failed to fetch mastery metadata; returning ids without names");
                return BTreeMap::new();
            }
        };
        if let Ok(json) = serde_json::to_string(&map) {
            self.cache.set(KEY, json, super::STATIC_TTL).await;
        }
        map
    }

    /// Fetch weekly raid clears, joined with the full encounter list so
    /// the LLM can answer "what raids do I still have left this week?"
    /// from a single tool call. Account clears cached `WALLET_TTL`;
    /// the raid metadata table is fetched once and cached under
    /// `STATIC_TTL`.
    pub async fn get_account_raids(
        &self,
        key: &ApiKey,
    ) -> Result<AccountRaidsSnapshot, ServiceError> {
        let cache_key = format!("account_raids:{}", key.fingerprint());
        let cleared: Vec<String> = if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str::<Vec<String>>(&json)
        {
            v
        } else {
            let v = self.gw2.fetch_account_raids(key).await?;
            if let Ok(json) = serde_json::to_string(&v) {
                self.cache.set(&cache_key, json, WALLET_TTL).await;
            }
            v
        };
        let metadata = self.raid_metadata_table().await;
        let cleared_set: BTreeSet<&str> = cleared.iter().map(String::as_str).collect();
        let now = self.clock.now();
        let mut encounters = Vec::new();
        let mut total = 0usize;
        for (raid_id, raid) in &metadata {
            let raid_name = title_case(raid_id);
            for wing in &raid.wings {
                let wing_name = title_case(&wing.id);
                for event in &wing.events {
                    total += 1;
                    encounters.push(RaidEncounterEntry {
                        id: event.id.clone(),
                        name: title_case(&event.id),
                        kind: event.kind.clone(),
                        wing_id: wing.id.clone(),
                        wing_name: wing_name.clone(),
                        raid_id: raid_id.clone(),
                        raid_name: raid_name.clone(),
                        cleared: cleared_set.contains(event.id.as_str()),
                    });
                }
            }
        }
        let cleared_count = encounters.iter().filter(|e| e.cleared).count();
        Ok(AccountRaidsSnapshot {
            encounters,
            cleared_count,
            total_count: total,
            weekly_reset_at: next_raid_reset(now),
            fetched_at: now,
        })
    }

    /// Fetch today's dungeon-path clears, joined with the full path list
    /// + next daily reset. Same shape as `get_account_raids` but on the
    ///   daily cadence instead of weekly.
    pub async fn get_account_dungeons(
        &self,
        key: &ApiKey,
    ) -> Result<AccountDungeonsSnapshot, ServiceError> {
        let cache_key = format!("account_dungeons:{}", key.fingerprint());
        let cleared: Vec<String> = if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str::<Vec<String>>(&json)
        {
            v
        } else {
            let v = self.gw2.fetch_account_dungeons(key).await?;
            if let Ok(json) = serde_json::to_string(&v) {
                self.cache.set(&cache_key, json, WALLET_TTL).await;
            }
            v
        };
        let metadata = self.dungeon_metadata_table().await;
        let cleared_set: BTreeSet<&str> = cleared.iter().map(String::as_str).collect();
        let now = self.clock.now();
        let mut paths = Vec::new();
        let mut total = 0usize;
        for (dungeon_id, dungeon) in &metadata {
            let dungeon_name = title_case(dungeon_id);
            for path in &dungeon.paths {
                total += 1;
                paths.push(DungeonPathEntry {
                    id: path.id.clone(),
                    name: title_case(&path.id),
                    kind: path.kind.clone(),
                    dungeon_id: dungeon_id.clone(),
                    dungeon_name: dungeon_name.clone(),
                    cleared: cleared_set.contains(path.id.as_str()),
                });
            }
        }
        let cleared_count = paths.iter().filter(|p| p.cleared).count();
        Ok(AccountDungeonsSnapshot {
            paths,
            cleared_count,
            total_count: total,
            daily_reset_at: next_daily_reset(now),
            fetched_at: now,
        })
    }

    /// Fetch + cache the full `/v2/raids` structure. Static — changes
    /// only with new wing releases — so one `STATIC_TTL` cache entry
    /// covers every consumer.
    async fn raid_metadata_table(&self) -> BTreeMap<String, Raid> {
        const KEY: &str = "raids:all";
        if let Some(json) = self.cache.get(KEY).await
            && let Ok(map) = serde_json::from_str::<BTreeMap<String, Raid>>(&json)
        {
            return map;
        }
        let ids = match self.gw2.fetch_all_raid_ids().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = ?e, "failed to enumerate raid ids; falling back to empty metadata");
                return BTreeMap::new();
            }
        };
        if ids.is_empty() {
            return BTreeMap::new();
        }
        let map = match self.gw2.fetch_raids(&ids).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "failed to fetch raid metadata; falling back to empty");
                return BTreeMap::new();
            }
        };
        if let Ok(json) = serde_json::to_string(&map) {
            self.cache.set(KEY, json, STATIC_TTL).await;
        }
        map
    }

    /// Sibling of [`raid_metadata_table`] for dungeons.
    async fn dungeon_metadata_table(&self) -> BTreeMap<String, Dungeon> {
        const KEY: &str = "dungeons:all";
        if let Some(json) = self.cache.get(KEY).await
            && let Ok(map) = serde_json::from_str::<BTreeMap<String, Dungeon>>(&json)
        {
            return map;
        }
        let ids = match self.gw2.fetch_all_dungeon_ids().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = ?e, "failed to enumerate dungeon ids; falling back to empty metadata");
                return BTreeMap::new();
            }
        };
        if ids.is_empty() {
            return BTreeMap::new();
        }
        let map = match self.gw2.fetch_dungeons(&ids).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "failed to fetch dungeon metadata; falling back to empty");
                return BTreeMap::new();
            }
        };
        if let Ok(json) = serde_json::to_string(&map) {
            self.cache.set(KEY, json, STATIC_TTL).await;
        }
        map
    }

    /// Fetch the daily / weekly / special Wizard's Vault track for the
    /// account. The per-account endpoints already embed
    /// title/track/acclaim so no catalog join is needed. Per-track
    /// cached `DAILIES_TTL` (1 hour) per key fingerprint — short enough
    /// that the rollover gets picked up promptly, long enough that we
    /// don't hammer the API on bursts of LLM tool calls.
    ///
    /// Replaces the deprecated `/v2/achievements/daily` endpoint, which
    /// returns 503 ("API not active") since the Wizard's Vault launch.
    pub async fn get_dailies(
        &self,
        key: &ApiKey,
        which: DailiesWhich,
    ) -> Result<WizardsVaultSnapshot, ServiceError> {
        let (track_label, cache_key) = match which {
            DailiesWhich::Daily => ("daily", format!("wizardsvault:daily:{}", key.fingerprint())),
            DailiesWhich::Weekly => (
                "weekly",
                format!("wizardsvault:weekly:{}", key.fingerprint()),
            ),
            DailiesWhich::Special => (
                "special",
                format!("wizardsvault:special:{}", key.fingerprint()),
            ),
        };
        let track: WizardsVaultTrack = if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(t) = serde_json::from_str::<WizardsVaultTrack>(&json)
        {
            debug!(track = track_label, "wizards-vault cache hit");
            t
        } else {
            let t = match which {
                DailiesWhich::Daily => self.gw2.fetch_wizards_vault_daily(key).await?,
                DailiesWhich::Weekly => self.gw2.fetch_wizards_vault_weekly(key).await?,
                DailiesWhich::Special => self.gw2.fetch_wizards_vault_special(key).await?,
            };
            if let Ok(json) = serde_json::to_string(&t) {
                self.cache.set(&cache_key, json, DAILIES_TTL).await;
            }
            t
        };
        Ok(WizardsVaultSnapshot::from_track(track))
    }

    pub(super) async fn fetch_currencies_for(
        &self,
        entries: &[WalletEntry],
    ) -> Result<BTreeMap<CurrencyId, Currency>, ServiceError> {
        let ids: Vec<CurrencyId> = entries.iter().map(|e| e.id).collect();
        self.get_currencies(&ids).await
    }

    /// Batch-resolve achievement metadata for a set of u32 ids. Best-effort:
    /// returns an empty map on any failure (so the caller falls back to
    /// id-only). Ids that don't validate as `AchievementId` (e.g. id=0)
    /// are silently dropped — the GW2 API never returns those anyway.
    async fn achievement_metadata_for_ids(
        &self,
        ids: impl IntoIterator<Item = u32>,
    ) -> BTreeMap<AchievementId, Achievement> {
        let typed: Vec<AchievementId> = ids
            .into_iter()
            .filter_map(|id| AchievementId::new(i64::from(id)).ok())
            .collect();
        if typed.is_empty() {
            return BTreeMap::new();
        }
        match self.get_achievements(&typed).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "failed to resolve achievement metadata; returning ids without names");
                BTreeMap::new()
            }
        }
    }
}

/// Look up name + description (truncated to one paragraph) for the given
/// raw id in a previously-fetched metadata map. Returns `(None, None)`
/// when the id wasn't included in the batch (validation failure or API
/// miss). Used by both `get_account_achievements` and `get_dailies`.
fn lookup_name_description(
    metadata: &BTreeMap<AchievementId, Achievement>,
    raw_id: u32,
) -> (Option<String>, Option<String>) {
    let Ok(typed) = AchievementId::new(i64::from(raw_id)) else {
        return (None, None);
    };
    let Some(a) = metadata.get(&typed) else {
        return (None, None);
    };
    let name = if a.name.is_empty() {
        None
    } else {
        Some(a.name.clone())
    };
    let description = a
        .extra
        .get("description")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    (name, description)
}

/// Result of `get_account_raids` — the full list of raid encounters with
/// a `cleared: bool` flag per encounter plus the next weekly reset
/// timestamp. Lets an LLM answer "what raids do I still have left this
/// week?" without a separate call.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountRaidsSnapshot {
    pub encounters: Vec<RaidEncounterEntry>,
    pub cleared_count: usize,
    pub total_count: usize,
    pub weekly_reset_at: DateTime<Utc>,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RaidEncounterEntry {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub wing_id: String,
    pub wing_name: String,
    pub raid_id: String,
    pub raid_name: String,
    pub cleared: bool,
}

/// Sibling of [`AccountRaidsSnapshot`] for dungeons. Daily cadence.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountDungeonsSnapshot {
    pub paths: Vec<DungeonPathEntry>,
    pub cleared_count: usize,
    pub total_count: usize,
    pub daily_reset_at: DateTime<Utc>,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DungeonPathEntry {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub dungeon_id: String,
    pub dungeon_name: String,
    pub cleared: bool,
}

/// An `AccountMastery { id, level }` joined with the matching mastery
/// track's canonical name and region from `/v2/masteries`. `name`,
/// `region`, and `current_level_name` are `Option` so a transient
/// lookup failure gracefully degrades to id-only.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountMasteryEntry {
    #[serde(flatten)]
    pub progress: AccountMastery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Name of the currently-unlocked tier (`levels[level - 1].name`).
    /// `None` when `level == 0` (track unlocked but no tier purchased
    /// yet) or when metadata wasn't available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_level_name: Option<String>,
}

fn enrich_mastery(
    progress: AccountMastery,
    metadata: &BTreeMap<crate::domain::MasteryId, crate::domain::Mastery>,
) -> AccountMasteryEntry {
    use crate::domain::MasteryId;
    let typed = MasteryId::new(i64::from(progress.id)).ok();
    let m = typed.and_then(|id| metadata.get(&id));
    let name = m.map(|m| m.name.clone()).filter(|s| !s.is_empty());
    let region = m.map(|m| m.region.clone()).filter(|s| !s.is_empty());
    let current_level_name = m.and_then(|m| {
        let level = progress.level;
        if level == 0 {
            return None;
        }
        let idx = usize::try_from(level - 1).ok()?;
        m.levels
            .get(idx)
            .map(|l| l.name.clone())
            .filter(|s| !s.is_empty())
    });
    AccountMasteryEntry {
        progress,
        name,
        region,
        current_level_name,
    }
}

/// An `AccountAchievement` row joined with the matching achievement's
/// canonical `name` and `description` from `/v2/achievements`. `name`
/// and `description` are `Option` so a transient lookup failure
/// gracefully degrades to id-only without poisoning the whole response.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountAchievementEntry {
    #[serde(flatten)]
    pub progress: AccountAchievement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Object wrapper around `Vec<String>` so the response is a JSON object
/// (MCP's `structuredContent` rejects bare arrays).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CharacterList {
    pub characters: Vec<String>,
    pub total: usize,
}

/// Result of `refresh_account_cache`. Kept tiny on purpose — the
/// caller doesn't need the actual cache-key strings (those would
/// leak an internal naming scheme and inflate tokens for no gain).
/// `cleared_count` is the operative answer; `cleared_at` lets the
/// LLM say "refreshed at HH:MM"; `scopes` lists the categories
/// cleared in human-readable terms.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RefreshAccountCacheResult {
    pub cleared_at: DateTime<Utc>,
    pub cleared_count: usize,
    pub scopes: Vec<String>,
}

/// Object wrapper around the per-account achievement list. `summary`
/// echoes the request parameter so the LLM can tell whether it's
/// looking at the filtered (in-progress only) or full list.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountAchievementsSnapshot {
    pub achievements: Vec<AccountAchievementEntry>,
    pub total: usize,
    pub summary: bool,
    pub fetched_at: DateTime<Utc>,
}

/// Object wrapper around the per-account mastery list.
/// `total_points_earned` sums the `point_cost` of every unlocked tier
/// across every track — when a track's metadata wasn't available, its
/// contribution falls back to 0 (the per-entry `current_level_name`
/// signal lets the LLM detect partial resolution).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountMasteriesSnapshot {
    pub masteries: Vec<AccountMasteryEntry>,
    pub total: usize,
    pub total_points_earned: u32,
    pub fetched_at: DateTime<Utc>,
}

fn total_mastery_points_earned(
    progress: &[AccountMastery],
    metadata: &BTreeMap<crate::domain::MasteryId, crate::domain::Mastery>,
) -> u32 {
    use crate::domain::MasteryId;
    progress
        .iter()
        .map(|p| {
            let Ok(typed) = MasteryId::new(i64::from(p.id)) else {
                return 0u32;
            };
            let Some(m) = metadata.get(&typed) else {
                return 0u32;
            };
            let Ok(take) = usize::try_from(p.level) else {
                return 0u32;
            };
            m.levels.iter().take(take).map(|l| l.point_cost).sum()
        })
        .sum()
}

fn wallet_cache_key(key: &ApiKey) -> String {
    format!("wallet:{}", key.fingerprint())
}

fn account_cache_key(key: &ApiKey) -> String {
    format!("account:{}", key.fingerprint())
}

/// `WizardsVaultTrack` plus derived acclaim totals so the LLM doesn't
/// have to sum `objectives[].acclaim` itself to answer "how much more
/// Astral Acclaim can I still earn this period?".
///
/// The track fields appear at the top level (via `#[serde(flatten)]`),
/// so this is wire-compatible with consumers that previously saw a
/// bare `WizardsVaultTrack` shape — they just gain three new fields.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct WizardsVaultSnapshot {
    #[serde(flatten)]
    pub track: WizardsVaultTrack,
    /// Total Astral Acclaim the player could still earn this period
    /// (sum of unclaimed objectives + the meta reward if unclaimed).
    /// Pairs with wallet AA to flag "you're going to cap soon."
    pub acclaim_remaining: u32,
    /// Astral Acclaim already collected from claimed objectives + an
    /// already-claimed meta reward. `acclaim_remaining + acclaim_earned
    /// == acclaim_total` always holds.
    pub acclaim_earned: u32,
    /// Total Astral Acclaim available across all objectives + the meta
    /// reward for this period. The denominator behind the LLM's "X%
    /// done" sentence.
    pub acclaim_total: u32,
}

impl WizardsVaultSnapshot {
    fn from_track(track: WizardsVaultTrack) -> Self {
        let mut earned: u32 = 0;
        let mut remaining: u32 = 0;
        for obj in &track.objectives {
            if obj.claimed {
                earned = earned.saturating_add(obj.acclaim);
            } else {
                remaining = remaining.saturating_add(obj.acclaim);
            }
        }
        if track.meta_reward_claimed {
            earned = earned.saturating_add(track.meta_reward_astral);
        } else {
            remaining = remaining.saturating_add(track.meta_reward_astral);
        }
        let total = earned.saturating_add(remaining);
        Self {
            track,
            acclaim_remaining: remaining,
            acclaim_earned: earned,
            acclaim_total: total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::WizardsVaultObjective;

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

    fn obj(acclaim: u32, claimed: bool) -> WizardsVaultObjective {
        WizardsVaultObjective {
            id: 1,
            title: String::new(),
            track: String::new(),
            acclaim,
            progress_current: 0,
            progress_complete: 1,
            claimed,
        }
    }

    fn track(
        objectives: Vec<WizardsVaultObjective>,
        meta: u32,
        meta_claimed: bool,
    ) -> WizardsVaultTrack {
        WizardsVaultTrack {
            meta_progress_current: 0,
            meta_progress_complete: 4,
            meta_reward_item_id: None,
            meta_reward_astral: meta,
            meta_reward_claimed: meta_claimed,
            objectives,
        }
    }

    #[test]
    fn vault_snapshot_sums_remaining_and_earned() {
        let snap = WizardsVaultSnapshot::from_track(track(
            vec![
                obj(25, true),  // claimed → earned
                obj(25, false), // unclaimed → remaining
                obj(50, false), // unclaimed → remaining
            ],
            50, // meta reward
            false,
        ));
        assert_eq!(snap.acclaim_earned, 25);
        assert_eq!(snap.acclaim_remaining, 25 + 50 + 50);
        assert_eq!(snap.acclaim_total, 25 + 25 + 50 + 50);
    }

    #[test]
    fn vault_snapshot_credits_claimed_meta_to_earned() {
        let snap =
            WizardsVaultSnapshot::from_track(track(vec![obj(25, true), obj(25, false)], 50, true));
        assert_eq!(snap.acclaim_earned, 25 + 50);
        assert_eq!(snap.acclaim_remaining, 25);
        assert_eq!(snap.acclaim_total, 100);
    }

    #[test]
    fn vault_snapshot_handles_fully_claimed_track() {
        let snap = WizardsVaultSnapshot::from_track(track(
            vec![obj(25, true), obj(25, true), obj(50, true)],
            50,
            true,
        ));
        assert_eq!(snap.acclaim_remaining, 0);
        assert_eq!(snap.acclaim_earned, 150);
        assert_eq!(snap.acclaim_total, 150);
    }

    #[test]
    fn vault_snapshot_serialises_track_fields_flat() {
        // `#[serde(flatten)]` must expose `meta_reward_astral` and
        // `objectives` at the top level so the wire shape stays
        // backward-compatible with consumers that read the bare track
        // shape before this snapshot was introduced.
        let snap = WizardsVaultSnapshot::from_track(track(vec![obj(25, false)], 50, false));
        let v = serde_json::to_value(&snap).unwrap();
        assert!(v.get("meta_reward_astral").is_some());
        assert!(v.get("objectives").is_some());
        assert_eq!(v["acclaim_remaining"], 25 + 50);
        assert_eq!(v["acclaim_total"], 75);
    }
}
