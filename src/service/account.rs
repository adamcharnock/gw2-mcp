//! Account-scoped endpoints — wallet, account, achievements, masteries,
//! raid/dungeon clears, dailies. All cached on `WALLET_TTL` (5 min)
//! except `get_dailies` which uses `DAILIES_TTL` (1 hour). Cache keys
//! derive from `key.fingerprint()` — the raw API key never touches a
//! cache key.

use std::collections::BTreeMap;

use tracing::{debug, warn};

use super::{DAILIES_TTL, Service, ServiceError, WALLET_TTL};
use crate::domain::{
    Account, AccountAchievement, AccountMastery, ApiKey, Currency, CurrencyId, Dailies,
    WalletEntry, WalletInfo,
};

/// Which day's dailies to fetch — `Today` hits `/v2/achievements/daily`,
/// `Tomorrow` hits `/v2/achievements/daily/tomorrow`. Defaults to today
/// because "what should I do?" almost always means right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DailiesWhich {
    #[default]
    Today,
    Tomorrow,
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

    pub(super) async fn fetch_currencies_for(
        &self,
        entries: &[WalletEntry],
    ) -> Result<BTreeMap<CurrencyId, Currency>, ServiceError> {
        let ids: Vec<CurrencyId> = entries.iter().map(|e| e.id).collect();
        self.get_currencies(&ids).await
    }
}

fn wallet_cache_key(key: &ApiKey) -> String {
    format!("wallet:{}", key.fingerprint())
}

fn account_cache_key(key: &ApiKey) -> String {
    format!("account:{}", key.fingerprint())
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
}
