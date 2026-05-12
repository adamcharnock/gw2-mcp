//! Account-scoped endpoints — wallet, account, achievements, masteries,
//! raid/dungeon clears, Wizard's Vault objectives. All cached on
//! `WALLET_TTL` (5 min) except `get_dailies` which uses `DAILIES_TTL`
//! (1 hour). Cache keys derive from `key.fingerprint()` — the raw API
//! key never touches a cache key.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::{DAILIES_TTL, STATIC_TTL, Service, ServiceError, WALLET_TTL, text_match};
use crate::domain::{
    Account, AccountAchievement, AccountMastery, Achievement, AchievementId, ApiKey,
    CharacterInventory, CharacterName, Currency, CurrencyId, Dungeon, InventorySlot, ItemId,
    MaterialCategory, MaterialSlot, Raid, WalletEntry, WalletInfo, WizardsVaultTrack,
    next_daily_reset, next_raid_reset, title_case,
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
                Ok(mut info) => {
                    debug!(fingerprint = %key.fingerprint(), "wallet cache hit");
                    // Cached `updated_at` is intentional; the delta has
                    // to be recomputed against now-time on every read so
                    // the LLM-visible value tracks wall clock, not when
                    // we cached.
                    info.updated_minutes_ago =
                        super::minutes_between(info.updated_at, self.clock.now());
                    return Ok(info);
                }
                // A poisoned cache entry should never block the request — log
                // and fall through to fetch fresh.
                Err(e) => warn!(error = ?e, "wallet cache poisoned; refetching"),
            }
        }

        debug!(fingerprint = %key.fingerprint(), "wallet cache miss");

        let raw_entries = self.gw2.fetch_wallet(key).await?;
        // Annotate cap-bearing currencies. Done in service (not the
        // HTTP adapter) so the cap table is a domain concern, not an
        // adapter concern.
        let entries: Vec<WalletEntry> = raw_entries
            .into_iter()
            .map(|mut e| {
                if let Some(cap) = crate::domain::currency_caps::get_cap(e.id) {
                    e.holding_cap = cap.holding_cap;
                    e.weekly_earn_cap = cap.weekly_earn_cap;
                    e.at_risk = crate::domain::currency_caps::is_at_risk(e.value, cap);
                }
                e
            })
            .collect();

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
            updated_minutes_ago: 0,
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
        // Summary mode trims two large per-row fields: the `bits` array
        // (often 100+ ints for an explorer / category-clearing
        // achievement; only the per-bit index, never readable text) and
        // the achievement description (helpful in full mode, noise in
        // the "what am I close to finishing?" scan that summary serves).
        let achievements: Vec<AccountAchievementEntry> = filtered
            .into_iter()
            .map(|mut progress| {
                if summary {
                    progress.bits = None;
                }
                let (name, description) = lookup_name_description(&metadata, progress.id);
                AccountAchievementEntry {
                    progress,
                    name,
                    description: if summary { None } else { description },
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

        // Best-effort: fetch + cache /v2/account/mastery/points. A
        // failure here leaves `points_by_region` empty rather than
        // poisoning the whole snapshot — the LLM still gets per-track
        // tiers from the main `masteries` array.
        let points_by_region = self.account_mastery_points(key).await;

        let masteries: Vec<AccountMasteryEntry> = progress
            .into_iter()
            .map(|p| enrich_mastery(p, &metadata))
            .collect();
        Ok(AccountMasteriesSnapshot {
            total: masteries.len(),
            total_points_earned,
            points_by_region,
            masteries,
            fetched_at: self.clock.now(),
        })
    }

    /// Fetch + cache `/v2/account/mastery/points`, project into
    /// per-region balances. Empty map on any error (logged at warn).
    async fn account_mastery_points(&self, key: &ApiKey) -> BTreeMap<String, MasteryPointBalance> {
        let cache_key = format!("account_mastery_points:{}", key.fingerprint());
        let raw: crate::domain::AccountMasteryPoints = if let Some(json) =
            self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str(&json)
        {
            v
        } else {
            match self.gw2.fetch_account_mastery_points(key).await {
                Ok(v) => {
                    if let Ok(json) = serde_json::to_string(&v) {
                        self.cache.set(&cache_key, json, WALLET_TTL).await;
                    }
                    v
                }
                Err(e) => {
                    warn!(error = ?e, "failed to fetch /v2/account/mastery/points; points_by_region unavailable");
                    return BTreeMap::new();
                }
            }
        };
        raw.totals
            .into_iter()
            .map(|r| {
                let unspent = i32::try_from(r.earned).unwrap_or(i32::MAX)
                    - i32::try_from(r.spent).unwrap_or(i32::MAX);
                (
                    r.region,
                    MasteryPointBalance {
                        earned: r.earned,
                        spent: r.spent,
                        unspent,
                    },
                )
            })
            .collect()
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
        // Nested raids -> wings -> encounters so the dungeon_id and
        // wing_id repetition that used to flood the flat encounter list
        // becomes a single field per parent grouping. Per-encounter
        // `name` was just title_case(id); dropped — the LLM can derive
        // it from the id when surfacing the result.
        let mut raids: Vec<RaidGroupEntry> = Vec::new();
        let mut total = 0usize;
        let mut cleared_count = 0usize;
        for (raid_id, raid) in &metadata {
            let mut wings_out: Vec<RaidWingEntry> = Vec::new();
            for wing in &raid.wings {
                let mut encounters: Vec<RaidEncounterEntry> = Vec::new();
                for event in &wing.events {
                    total += 1;
                    let was_cleared = cleared_set.contains(event.id.as_str());
                    if was_cleared {
                        cleared_count += 1;
                    }
                    encounters.push(RaidEncounterEntry {
                        id: event.id.clone(),
                        kind: event.kind.clone(),
                        cleared: was_cleared,
                    });
                }
                wings_out.push(RaidWingEntry {
                    id: wing.id.clone(),
                    name: title_case(&wing.id),
                    encounters,
                });
            }
            raids.push(RaidGroupEntry {
                id: raid_id.clone(),
                name: title_case(raid_id),
                wings: wings_out,
            });
        }
        let weekly_reset_at = next_raid_reset(now);
        Ok(AccountRaidsSnapshot {
            raids,
            cleared_count,
            total_count: total,
            weekly_reset_at,
            weekly_reset_in_minutes: super::minutes_between(now, weekly_reset_at),
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
        // Nested dungeons -> paths shape: the dungeon_id repetition that
        // existed on every path under the old flat list collapses into
        // one field per dungeon. Per-path `name` was just
        // title_case(id); the LLM derives it on demand from the id.
        let mut dungeons: Vec<DungeonGroupEntry> = Vec::new();
        let mut total = 0usize;
        let mut cleared_count = 0usize;
        for (dungeon_id, dungeon) in &metadata {
            let mut paths_out: Vec<DungeonPathEntry> = Vec::new();
            for path in &dungeon.paths {
                total += 1;
                let was_cleared = cleared_set.contains(path.id.as_str());
                if was_cleared {
                    cleared_count += 1;
                }
                paths_out.push(DungeonPathEntry {
                    id: path.id.clone(),
                    kind: path.kind.clone(),
                    cleared: was_cleared,
                });
            }
            dungeons.push(DungeonGroupEntry {
                id: dungeon_id.clone(),
                name: title_case(dungeon_id),
                paths: paths_out,
            });
        }
        let daily_reset_at = next_daily_reset(now);
        Ok(AccountDungeonsSnapshot {
            dungeons,
            cleared_count,
            total_count: total,
            daily_reset_at,
            daily_reset_in_minutes: super::minutes_between(now, daily_reset_at),
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

    /// Fetch the account bank and project it for LLM consumption.
    ///
    /// Summary mode (`summary=true`, default): collapses to one row per
    /// unique item id with summed counts. Strips binding/charges. Use
    /// case: "what do I have stockpiled?".
    ///
    /// Full mode (`summary=false`): one row per occupied slot, with
    /// binding + charges preserved. Use case: "find my soulbound legendaries"
    /// or "which slots are filled with what".
    ///
    /// Item names are pre-resolved via `/v2/items` so the LLM doesn't
    /// have to follow up with `get_items`. Name lookup is best-effort
    /// — a failure leaves `name: None` rather than poisoning the whole
    /// response.
    pub async fn get_account_bank(
        &self,
        key: &ApiKey,
        summary: bool,
        filter: &StorageFilter,
    ) -> Result<AccountBankSnapshot, ServiceError> {
        let cache_key = format!("account_bank:{}", key.fingerprint());
        let slots: Vec<InventorySlot> = if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str::<Vec<InventorySlot>>(&json)
        {
            v
        } else {
            let v = self.gw2.fetch_account_bank(key).await?;
            if let Ok(json) = serde_json::to_string(&v) {
                self.cache.set(&cache_key, json, WALLET_TTL).await;
            }
            v
        };

        let want_ids: Option<BTreeSet<u32>> = filter
            .item_ids
            .as_ref()
            .map(|v| v.iter().copied().collect());

        // Item-name resolution — collect unique ids, batch-resolve.
        // Include any explicitly-requested item_ids so count: 0 rows
        // still carry a name.
        let unique_ids: Vec<ItemId> = {
            let mut set = BTreeSet::new();
            for s in &slots {
                if let Ok(id) = ItemId::new(i64::from(s.id)) {
                    set.insert(id);
                }
            }
            if let Some(ids) = &want_ids {
                for &id in ids {
                    if let Ok(id) = ItemId::new(i64::from(id)) {
                        set.insert(id);
                    }
                }
            }
            set.into_iter().collect()
        };
        let names = self.item_names_for(&unique_ids).await;

        let mut items: Vec<BankItemEntry> = if summary {
            // Group by id, sum counts. Order by descending count so the
            // LLM sees biggest stockpiles first.
            let mut by_id: BTreeMap<u32, u32> = BTreeMap::new();
            for s in &slots {
                *by_id.entry(s.id).or_default() += s.count;
            }
            let mut entries: Vec<BankItemEntry> = by_id
                .into_iter()
                .map(|(id, total)| BankItemEntry {
                    id,
                    name: names.get(&id).cloned(),
                    count: total,
                    binding: None,
                    bound_to: None,
                    charges: None,
                })
                .collect();
            entries.sort_by(|a, b| b.count.cmp(&a.count).then(a.id.cmp(&b.id)));
            entries
        } else {
            slots
                .iter()
                .map(|s| BankItemEntry {
                    id: s.id,
                    name: names.get(&s.id).cloned(),
                    count: s.count,
                    binding: s.binding.clone(),
                    bound_to: s.bound_to.clone(),
                    charges: s.charges,
                })
                .collect()
        };

        // Apply filters. item_ids → keep only matching rows AND inject
        // count: 0 for ids that weren't present at all. name_contains
        // → filter by item name (skips rows with no resolved name —
        // best-effort enrichment, treated as a non-match).
        if let Some(ids) = &want_ids {
            items.retain(|e| ids.contains(&e.id));
            let present: BTreeSet<u32> = items.iter().map(|e| e.id).collect();
            for &id in ids {
                if !present.contains(&id) {
                    items.push(BankItemEntry {
                        id,
                        name: names.get(&id).cloned(),
                        count: 0,
                        binding: None,
                        bound_to: None,
                        charges: None,
                    });
                }
            }
            items.sort_by(|a, b| b.count.cmp(&a.count).then(a.id.cmp(&b.id)));
        }
        if let Some(needle) = filter.name_contains.as_deref() {
            items.retain(|e| {
                e.name
                    .as_deref()
                    .is_some_and(|n| text_match::name_contains_match(needle, n))
            });
        }

        Ok(AccountBankSnapshot {
            summary,
            unique_item_count: unique_ids.len(),
            used_slots: slots.len(),
            items,
            fetched_at: self.clock.now(),
        })
    }

    /// Fetch the account material storage and project it for the LLM.
    ///
    /// See [`StorageFilter`] for filtering semantics shared with the
    /// bank and inventory tools.
    ///
    /// Summary mode (`summary=true`, default): drops count=0 rows (most
    /// of the materials list — empty slots dominate). Sorted by count
    /// descending. Drops binding.
    ///
    /// Full mode (`summary=false`): every slot including count=0,
    /// binding preserved.
    ///
    /// Item names + category names are pre-resolved (best-effort).
    ///
    /// Filters compose: `item_ids` AND `categories` AND `name_contains`
    /// all narrow the result; passing none returns the full storage.
    /// When `item_ids` is set, every requested id appears in the
    /// response even if it's not in storage (count: 0), so the caller
    /// can distinguish "you have N" from "you don't have this".
    #[allow(clippy::too_many_lines)] // grouping + filter passes
    pub async fn get_account_materials(
        &self,
        key: &ApiKey,
        summary: bool,
        filter: &StorageFilter,
    ) -> Result<AccountMaterialsSnapshot, ServiceError> {
        let cache_key = format!("account_materials:{}", key.fingerprint());
        let slots: Vec<MaterialSlot> = if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str::<Vec<MaterialSlot>>(&json)
        {
            v
        } else {
            let v = self.gw2.fetch_account_materials(key).await?;
            if let Ok(json) = serde_json::to_string(&v) {
                self.cache.set(&cache_key, json, WALLET_TTL).await;
            }
            v
        };

        // Build a quick lookup of the slot data for the count: 0
        // injection path when item_ids is set.
        let slots_by_id: BTreeMap<u32, &MaterialSlot> = slots.iter().map(|s| (s.id, s)).collect();
        let want_ids: Option<BTreeSet<u32>> = filter
            .item_ids
            .as_ref()
            .map(|v| v.iter().copied().collect());
        let want_categories: Option<BTreeSet<u32>> = filter
            .categories
            .as_ref()
            .map(|v| v.iter().copied().collect());

        // Best-effort enrichment: item names. The set includes all
        // slot ids that will pass the summary gate, plus any item_ids
        // requested that aren't in storage (so the count: 0 rows get
        // names too).
        let unique_item_ids: Vec<ItemId> = {
            let mut set = BTreeSet::new();
            for s in &slots {
                if s.count == 0 && summary {
                    continue;
                }
                if let Ok(id) = ItemId::new(i64::from(s.id)) {
                    set.insert(id);
                }
            }
            if let Some(ids) = &want_ids {
                for &id in ids {
                    if let Ok(id) = ItemId::new(i64::from(id)) {
                        set.insert(id);
                    }
                }
            }
            set.into_iter().collect()
        };
        let item_names = self.item_names_for(&unique_item_ids).await;
        let category_table = self.material_category_table().await;

        // Group slots into categories[].items[]. The flat shape carried
        // category_name on every row (293× redundancy on a real
        // account). One copy per category here.
        let mut by_category: BTreeMap<u32, Vec<MaterialItemEntry>> = BTreeMap::new();

        // Pass 1: surface slots that pass summary + filter gates.
        for s in &slots {
            if summary && s.count == 0 {
                // Exception: when item_ids is set, the count: 0 rows
                // for explicitly-requested ids should still appear
                // (handled in pass 2). Skip here.
                if want_ids.as_ref().is_none_or(|ids| !ids.contains(&s.id)) {
                    continue;
                }
            }
            if let Some(ids) = &want_ids
                && !ids.contains(&s.id)
            {
                continue;
            }
            if let Some(cats) = &want_categories
                && !cats.contains(&s.category)
            {
                continue;
            }
            let name = item_names.get(&s.id).cloned();
            if let Some(needle) = filter.name_contains.as_deref() {
                let hay = name.as_deref().unwrap_or("");
                if !text_match::name_contains_match(needle, hay) {
                    continue;
                }
            }
            by_category
                .entry(s.category)
                .or_default()
                .push(MaterialItemEntry {
                    id: s.id,
                    name,
                    count: s.count,
                    binding: if summary { None } else { s.binding.clone() },
                });
        }

        // Pass 2: inject count: 0 rows for requested item_ids that
        // weren't in storage at all. These tell the caller "you don't
        // have this" rather than "I forgot to include it".
        if let Some(ids) = &want_ids {
            for &id in ids {
                let already_present = by_category.values().any(|v| v.iter().any(|e| e.id == id));
                if already_present {
                    continue;
                }
                // Determine category for the missing id. Slot data
                // gives us the canonical category when the slot exists
                // with count: 0; otherwise we file it under category 0
                // as a sentinel ("requested but not in storage").
                let (category, count) = slots_by_id
                    .get(&id)
                    .map_or((0, 0), |s| (s.category, s.count));
                if let Some(cats) = &want_categories
                    && !cats.contains(&category)
                {
                    continue;
                }
                let name = item_names.get(&id).cloned();
                if let Some(needle) = filter.name_contains.as_deref() {
                    let hay = name.as_deref().unwrap_or("");
                    if !text_match::name_contains_match(needle, hay) {
                        continue;
                    }
                }
                by_category
                    .entry(category)
                    .or_default()
                    .push(MaterialItemEntry {
                        id,
                        name,
                        count,
                        binding: None,
                    });
            }
        }

        // Order: categories by name (then id as tiebreaker for missing
        // metadata); items within a category by count desc, id asc.
        let mut categories: Vec<MaterialCategoryGroup> = by_category
            .into_iter()
            .map(|(cat, mut items)| {
                items.sort_by(|a, b| b.count.cmp(&a.count).then(a.id.cmp(&b.id)));
                MaterialCategoryGroup {
                    category: cat,
                    name: category_table.get(&cat).map(|c| c.name.clone()),
                    items,
                }
            })
            .collect();
        categories.sort_by(|a, b| {
            a.name
                .as_deref()
                .unwrap_or("")
                .cmp(b.name.as_deref().unwrap_or(""))
                .then(a.category.cmp(&b.category))
        });

        let total_slots = slots.len();
        let used_slots = slots.iter().filter(|s| s.count > 0).count();

        Ok(AccountMaterialsSnapshot {
            summary,
            categories,
            used_slots,
            total_slots,
            fetched_at: self.clock.now(),
        })
    }

    /// Fetch a character's bag inventory. Same summary-vs-full pattern
    /// as `get_account_bank`. See [`StorageFilter`] for filtering
    /// semantics. The character must exist on the account for this key.
    #[allow(clippy::too_many_lines)] // bag flatten + filter passes
    pub async fn get_character_inventory(
        &self,
        key: &ApiKey,
        name: &CharacterName,
        summary: bool,
        filter: &StorageFilter,
    ) -> Result<CharacterInventorySnapshot, ServiceError> {
        let cache_key = format!(
            "character_inventory:{}:{}",
            key.fingerprint(),
            name.as_str()
        );
        let inv: CharacterInventory = if let Some(json) = self.cache.get(&cache_key).await
            && let Ok(v) = serde_json::from_str::<CharacterInventory>(&json)
        {
            v
        } else {
            let v = self.gw2.fetch_character_inventory(key, name).await?;
            if let Ok(json) = serde_json::to_string(&v) {
                self.cache.set(&cache_key, json, WALLET_TTL).await;
            }
            v
        };

        // Flatten all bags into one occupied-slot list. We don't surface
        // per-bag structure — it's seldom what the LLM is asked about
        // ("how many ascended chests do I have on this char?" doesn't
        // care which bag they're in).
        let occupied: Vec<&InventorySlot> = inv
            .bags
            .iter()
            .flatten()
            .flat_map(|bag| bag.inventory.iter().flatten())
            .collect();
        let total_slots: usize = inv.bags.iter().flatten().map(|b| b.size as usize).sum();

        let want_ids: Option<BTreeSet<u32>> = filter
            .item_ids
            .as_ref()
            .map(|v| v.iter().copied().collect());

        // Item-name resolution. Include any explicitly-requested
        // item_ids so count: 0 rows still carry a name.
        let unique_ids: Vec<ItemId> = {
            let mut set = BTreeSet::new();
            for s in &occupied {
                if let Ok(id) = ItemId::new(i64::from(s.id)) {
                    set.insert(id);
                }
            }
            if let Some(ids) = &want_ids {
                for &id in ids {
                    if let Ok(id) = ItemId::new(i64::from(id)) {
                        set.insert(id);
                    }
                }
            }
            set.into_iter().collect()
        };
        let names = self.item_names_for(&unique_ids).await;

        let mut items: Vec<BankItemEntry> = if summary {
            let mut by_id: BTreeMap<u32, u32> = BTreeMap::new();
            for s in &occupied {
                *by_id.entry(s.id).or_default() += s.count;
            }
            let mut entries: Vec<BankItemEntry> = by_id
                .into_iter()
                .map(|(id, total)| BankItemEntry {
                    id,
                    name: names.get(&id).cloned(),
                    count: total,
                    binding: None,
                    bound_to: None,
                    charges: None,
                })
                .collect();
            entries.sort_by(|a, b| b.count.cmp(&a.count).then(a.id.cmp(&b.id)));
            entries
        } else {
            occupied
                .iter()
                .map(|s| BankItemEntry {
                    id: s.id,
                    name: names.get(&s.id).cloned(),
                    count: s.count,
                    binding: s.binding.clone(),
                    bound_to: s.bound_to.clone(),
                    charges: s.charges,
                })
                .collect()
        };

        // Apply filters. See get_account_bank for the parallel logic.
        if let Some(ids) = &want_ids {
            items.retain(|e| ids.contains(&e.id));
            let present: BTreeSet<u32> = items.iter().map(|e| e.id).collect();
            for &id in ids {
                if !present.contains(&id) {
                    items.push(BankItemEntry {
                        id,
                        name: names.get(&id).cloned(),
                        count: 0,
                        binding: None,
                        bound_to: None,
                        charges: None,
                    });
                }
            }
            items.sort_by(|a, b| b.count.cmp(&a.count).then(a.id.cmp(&b.id)));
        }
        if let Some(needle) = filter.name_contains.as_deref() {
            items.retain(|e| {
                e.name
                    .as_deref()
                    .is_some_and(|n| text_match::name_contains_match(needle, n))
            });
        }

        Ok(CharacterInventorySnapshot {
            character_name: name.as_str().to_owned(),
            summary,
            unique_item_count: unique_ids.len(),
            used_slots: occupied.len(),
            total_slots,
            items,
            fetched_at: self.clock.now(),
        })
    }

    /// Fetch + cache the full `/v2/materials` table. Static — only
    /// changes with new expansions — so one `STATIC_TTL` cache entry
    /// covers every consumer. Empty map on any failure.
    async fn material_category_table(&self) -> BTreeMap<u32, MaterialCategory> {
        const KEY: &str = "material_categories:all";
        if let Some(json) = self.cache.get(KEY).await
            && let Ok(map) = serde_json::from_str::<BTreeMap<u32, MaterialCategory>>(&json)
        {
            return map;
        }
        let ids = match self.gw2.fetch_all_material_category_ids().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = ?e, "failed to enumerate material category ids; category names unavailable");
                return BTreeMap::new();
            }
        };
        if ids.is_empty() {
            return BTreeMap::new();
        }
        let map = match self.gw2.fetch_material_categories(&ids).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, "failed to fetch material categories; returning ids without names");
                return BTreeMap::new();
            }
        };
        if let Ok(json) = serde_json::to_string(&map) {
            self.cache.set(KEY, json, STATIC_TTL).await;
        }
        map
    }

    /// Batch-resolve `ItemId -> name` for a set of ids. Best-effort —
    /// empty map on any failure. Skips ids that don't validate.
    async fn item_names_for(&self, ids: &[ItemId]) -> BTreeMap<u32, String> {
        if ids.is_empty() {
            return BTreeMap::new();
        }
        match self.get_items(ids).await {
            Ok(m) => m
                .into_iter()
                .map(|(id, item)| (id.get(), item.name))
                .collect(),
            Err(e) => {
                warn!(error = ?e, "failed to resolve item names; bank entries will use id only");
                BTreeMap::new()
            }
        }
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

/// Result of `get_account_raids`. Nested raid → wing → encounter shape:
/// each encounter is identified by id-only with its kind + cleared flag;
/// the parent `raid_id` / `wing_id` are factored out into the grouping so the
/// list of ~60 encounters doesn't repeat them. Lets an LLM answer "what
/// raids do I still have left this week?" without a separate call.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountRaidsSnapshot {
    pub raids: Vec<RaidGroupEntry>,
    pub cleared_count: usize,
    pub total_count: usize,
    pub weekly_reset_at: DateTime<Utc>,
    /// Minutes until `weekly_reset_at`. Sibling of the absolute
    /// timestamp per the time-delta convention (see
    /// `service::minutes_between`); the LLM should prefer this over
    /// computing from `weekly_reset_at` itself.
    pub weekly_reset_in_minutes: i64,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RaidGroupEntry {
    pub id: String,
    pub name: String,
    pub wings: Vec<RaidWingEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RaidWingEntry {
    pub id: String,
    pub name: String,
    pub encounters: Vec<RaidEncounterEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RaidEncounterEntry {
    pub id: String,
    pub kind: String,
    pub cleared: bool,
}

/// Sibling of [`AccountRaidsSnapshot`] for dungeons. Same nested shape
/// (dungeon → path), daily cadence instead of weekly.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountDungeonsSnapshot {
    pub dungeons: Vec<DungeonGroupEntry>,
    pub cleared_count: usize,
    pub total_count: usize,
    pub daily_reset_at: DateTime<Utc>,
    /// Minutes until `daily_reset_at`. Sibling of the absolute
    /// timestamp per the time-delta convention.
    pub daily_reset_in_minutes: i64,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DungeonGroupEntry {
    pub id: String,
    pub name: String,
    pub paths: Vec<DungeonPathEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DungeonPathEntry {
    pub id: String,
    pub kind: String,
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
///
/// `total_points_earned` is a legacy field — it actually sums the
/// `point_cost` of every unlocked tier across every track (i.e. points
/// SPENT, not earned, despite the name). Kept for wire compatibility;
/// new callers should prefer the `points_by_region` breakdown which
/// pulls accurate {earned, spent, unspent} numbers from
/// `/v2/account/mastery/points`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountMasteriesSnapshot {
    pub masteries: Vec<AccountMasteryEntry>,
    pub total: usize,
    pub total_points_earned: u32,
    /// Per-region {earned, spent, unspent} mastery-point balances.
    /// Sourced directly from `/v2/account/mastery/points`; the LLM
    /// can use this to answer "which mastery track can I afford to
    /// finish?" by comparing `unspent` against the next tier's
    /// `point_cost` on each track in that region.
    ///
    /// Empty when the points endpoint failed (best-effort enrichment,
    /// like the metadata table).
    pub points_by_region: BTreeMap<String, MasteryPointBalance>,
    pub fetched_at: DateTime<Utc>,
}

/// Per-region mastery-point balance. `unspent` is `earned - spent`,
/// signed so a misconfiguration ("API says spent > earned") surfaces
/// instead of underflowing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MasteryPointBalance {
    pub earned: u32,
    pub spent: u32,
    pub unspent: i32,
}

/// Shared filter knobs for the three storage-tab tools
/// (`get_account_materials`, `get_account_bank`,
/// `get_character_inventory`). All fields are optional and compose
/// with AND semantics — passing `Default::default()` returns the full
/// storage. Distinct from `summary` (which is a presentation flag,
/// not a content filter).
///
/// `item_ids` has a special wrinkle: when set, every requested id
/// appears in the response even if storage doesn't contain it (the
/// row will have `count: 0`). That makes "do I have any?" answerable
/// from a single call without the caller having to reason about
/// missing rows. `categories` is honored only by
/// `get_account_materials` (bank + inventory don't expose category
/// metadata).
#[derive(Debug, Clone, Default)]
pub struct StorageFilter {
    /// Restrict to these GW2 item ids. When set, the response also
    /// includes a `count: 0` row for any requested id not in storage.
    pub item_ids: Option<Vec<u32>>,
    /// Restrict to these material category ids
    /// (`get_account_materials` only — ignored elsewhere).
    pub categories: Option<Vec<u32>>,
    /// Case-insensitive, token-based substring match on the item
    /// name. See `text_match::name_contains_match`.
    pub name_contains: Option<String>,
}

/// Result of `get_account_bank`. See `Service::get_account_bank` for
/// summary-vs-full shape semantics.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountBankSnapshot {
    /// Echoes the request arg. Lets the LLM disambiguate at call time
    /// without re-checking its arguments.
    pub summary: bool,
    /// Number of distinct item ids found across the bank.
    pub unique_item_count: usize,
    /// Number of occupied bank slots (after null-filtering by adapter).
    pub used_slots: usize,
    pub items: Vec<BankItemEntry>,
    pub fetched_at: DateTime<Utc>,
}

/// One row in the bank response. In summary mode each row aggregates a
/// single item id across all slots (binding/charges are dropped). In
/// full mode each row is one slot (binding/charges preserved).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BankItemEntry {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charges: Option<u32>,
}

/// Result of `get_character_inventory`. Bag structure is flattened —
/// the LLM gets a single per-character item list, not per-bag.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CharacterInventorySnapshot {
    pub character_name: String,
    pub summary: bool,
    pub unique_item_count: usize,
    pub used_slots: usize,
    pub total_slots: usize,
    /// Reuses `BankItemEntry` because the per-row shape is identical
    /// (id, name, count, optional binding/charges).
    pub items: Vec<BankItemEntry>,
    pub fetched_at: DateTime<Utc>,
}

/// Result of `get_account_materials`. See `Service::get_account_materials`
/// for summary-vs-full shape semantics.
///
/// Items are nested under their category (`categories[].items[]`) to
/// avoid emitting `category_name` once per row — the original flat
/// shape carried ~293 redundant copies of the same handful of strings.
/// Mirrors the structure used by `get_account_raids` /
/// `get_account_dungeons`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AccountMaterialsSnapshot {
    pub summary: bool,
    pub categories: Vec<MaterialCategoryGroup>,
    /// Slots with `count > 0`. The denominator of fill ratio.
    pub used_slots: usize,
    /// Every slot the material storage tab has, even count=0 ones.
    pub total_slots: usize,
    pub fetched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MaterialCategoryGroup {
    /// Material category id from the GW2 API (`/v2/materials`).
    pub category: u32,
    /// Human-readable category name (`"Basic Crafting Materials"`,
    /// `"Festive Materials"`, …). `None` only if the metadata catalog
    /// failed to load — graceful degradation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub items: Vec<MaterialItemEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MaterialItemEntry {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
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

    #[test]
    fn mastery_point_balance_unspent_computes_correctly() {
        let b = MasteryPointBalance {
            earned: 51,
            spent: 49,
            unspent: 2,
        };
        // Round-trip via serde to ensure the wire shape is stable.
        let v = serde_json::to_value(b).unwrap();
        assert_eq!(v["earned"], 51);
        assert_eq!(v["spent"], 49);
        assert_eq!(v["unspent"], 2);
    }
}
