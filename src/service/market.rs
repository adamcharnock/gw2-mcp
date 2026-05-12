//! Trading-post pricing service. Wraps `/v2/commerce/prices` with
//! short-TTL caching and item-name enrichment.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::{MARKET_TTL, Service, ServiceError};
use crate::domain::{ItemId, MarketPrice, PriceOrderbook};

impl Service {
    /// Bulk-fetch trading-post prices for the given item ids.
    /// Each id is cached independently at `MARKET_TTL` (60s) so
    /// overlapping subsequent calls only pay the network cost for the
    /// uncached subset.
    ///
    /// Coin values are in copper: gold = price / 10000, silver =
    /// (price % 10000) / 100. The tool description must surface this
    /// to the LLM.
    ///
    /// Item names are pre-resolved (best-effort) so the LLM doesn't
    /// have to chain `get_items`. Ids that don't have market data
    /// (un-tradeable items) are silently omitted from the response.
    pub async fn get_market_prices(
        &self,
        ids: &[ItemId],
    ) -> Result<MarketPricesResponse, ServiceError> {
        if ids.is_empty() {
            return Ok(MarketPricesResponse {
                items: Vec::new(),
                total: 0,
                fetched_at: self.clock.now(),
                fetched_minutes_ago: 0,
            });
        }

        // De-duplicate input to avoid wasted cache lookups + HTTP work.
        let unique: Vec<ItemId> = {
            let mut set: BTreeSet<ItemId> = BTreeSet::new();
            set.extend(ids.iter().copied());
            set.into_iter().collect()
        };

        // Per-id cache check.
        let mut hits: BTreeMap<ItemId, MarketPrice> = BTreeMap::new();
        let mut misses: Vec<ItemId> = Vec::new();
        for id in &unique {
            let cache_key = format!("market_price:{}", id.get());
            if let Some(json) = self.cache.get(&cache_key).await
                && let Ok(p) = serde_json::from_str::<MarketPrice>(&json)
            {
                hits.insert(*id, p);
            } else {
                misses.push(*id);
            }
        }
        debug!(
            requested = unique.len(),
            hit = hits.len(),
            "market price cache lookup"
        );

        // Single bulk fetch for the misses.
        if !misses.is_empty() {
            let fresh = self.gw2.fetch_market_prices(&misses).await?;
            for (id, price) in fresh {
                let cache_key = format!("market_price:{}", id.get());
                if let Ok(json) = serde_json::to_string(&price) {
                    self.cache.set(&cache_key, json, MARKET_TTL).await;
                }
                hits.insert(id, price);
            }
        }

        // Name resolution — best-effort.
        let id_vec: Vec<ItemId> = hits.keys().copied().collect();
        let names = match self.get_items(&id_vec).await {
            Ok(m) => m
                .into_iter()
                .map(|(id, item)| (id, item.name))
                .collect::<BTreeMap<ItemId, String>>(),
            Err(e) => {
                warn!(error = ?e, "failed to resolve item names; market prices will use id only");
                BTreeMap::new()
            }
        };

        let mut items: Vec<MarketPriceEntry> = hits
            .into_iter()
            .map(|(id, p)| MarketPriceEntry {
                id,
                name: names.get(&id).cloned(),
                whitelisted: p.whitelisted,
                buys: p.buys,
                sells: p.sells,
            })
            .collect();
        // Ascending id for stable diffs and tests.
        items.sort_by(|a, b| a.id.cmp(&b.id));

        Ok(MarketPricesResponse {
            total: items.len(),
            items,
            fetched_at: self.clock.now(),
            fetched_minutes_ago: 0,
        })
    }
}

/// Top-level `get_market_prices` response.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MarketPricesResponse {
    pub items: Vec<MarketPriceEntry>,
    pub total: usize,
    pub fetched_at: DateTime<Utc>,
    pub fetched_minutes_ago: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MarketPriceEntry {
    pub id: ItemId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub whitelisted: bool,
    pub buys: PriceOrderbook,
    pub sells: PriceOrderbook,
}
