//! Reference data — currencies, skills, traits, specializations, items.
//!
//! All cached per-id under `STATIC_TTL` (1 year). The data is effectively
//! static — game patches re-fetch when the binary updates. Reference data
//! is bulk-fetched (`ids=...` batched) and stored one cache entry per id
//! so concurrent overlapping requests share work.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tracing::warn;

use super::{STATIC_TTL, Service, ServiceError};
use crate::domain::{
    Currency, CurrencyId, Item, ItemId, Skill, SkillId, Specialization, SpecializationId, Trait,
    TraitId,
};
use crate::ports::Cache;

impl Service {
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
}

fn currency_cache_key(id: CurrencyId) -> String {
    format!("currency:detail:{id}")
}

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
