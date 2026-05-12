//! Trading-post pricing types. Wire shape of `/v2/commerce/prices`.
//!
//! No API key required — these endpoints are public. Prices are
//! volatile (minute-to-minute) so the service uses a short cache TTL.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::reference::ItemId;

/// One row from `/v2/commerce/prices`. `unit_price` is in copper (gold
/// = price / 10000). Buy orders sit at "highest someone will pay";
/// sell orders at "lowest someone will sell for".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MarketPrice {
    pub id: ItemId,
    /// True if the item is tradeable on F2P accounts. Most useful as a
    /// signal that the item is whitelisted at all (i.e. tradeable).
    #[serde(default)]
    pub whitelisted: bool,
    pub buys: PriceOrderbook,
    pub sells: PriceOrderbook,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PriceOrderbook {
    /// Highest buy or lowest sell, in copper.
    pub unit_price: i64,
    /// Total quantity at all price points (not just at `unit_price`).
    pub quantity: i64,
}
