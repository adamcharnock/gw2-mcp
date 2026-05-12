//! Wallet types from the GW2 `/v2/account/wallet` endpoint.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::currency::{Currency, CurrencyId};

/// A single wallet line item: how much of one currency the account holds.
///
/// Cap fields (`holding_cap`, `weekly_earn_cap`, `at_risk`) are populated
/// from `domain::currency_caps` when the currency has a documented cap;
/// `None` for currencies without one. `at_risk` is true when balance is
/// at or above 80% of `holding_cap` — the LLM should use this to flag
/// "you'll start forfeiting new earnings, spend down" warnings.
/// Capped currencies generally do NOT expire; they just block further
/// earning at the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WalletEntry {
    pub id: CurrencyId,
    /// Quantity. Coin is in copper; other currencies use their natural unit.
    pub value: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holding_cap: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_earn_cap: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_risk: Option<bool>,
}

/// Aggregated wallet view returned to MCP clients.
///
/// Uses `BTreeMap` so JSON output is stable across runs — useful for
/// caching, snapshots, and tests.
///
/// `updated_minutes_ago` is recomputed on every read (not cached) so
/// the delta reflects wall-clock-now, not the moment we cached. See
/// `service::minutes_between` for the convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WalletInfo {
    pub entries: Vec<WalletEntry>,
    pub currencies: BTreeMap<CurrencyId, Currency>,
    pub total_currencies: usize,
    pub updated_at: DateTime<Utc>,
    /// Minutes since `updated_at`. Recomputed at response time; never
    /// cached. Negative would mean `updated_at` is in the future
    /// (clock skew); the field stays signed for symmetry with future
    /// `*_in_minutes` reset deltas.
    #[serde(default)]
    pub updated_minutes_ago: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_entry_deserialises_from_gw2_payload() {
        let raw = r#"[{"id":1,"value":12345}]"#;
        let entries: Vec<WalletEntry> = serde_json::from_str(raw).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, CurrencyId::new(1).unwrap());
        assert_eq!(entries[0].value, 12345);
        // Cap fields default to None when the GW2 payload doesn't
        // carry them (the wire format never does — the service layer
        // populates them post-fetch).
        assert!(entries[0].holding_cap.is_none());
        assert!(entries[0].at_risk.is_none());
    }

    #[test]
    fn wallet_info_round_trip() {
        let info = WalletInfo {
            entries: vec![WalletEntry {
                id: CurrencyId::new(1).unwrap(),
                value: 100,
                holding_cap: None,
                weekly_earn_cap: None,
                at_risk: None,
            }],
            currencies: BTreeMap::new(),
            total_currencies: 1,
            updated_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            updated_minutes_ago: 0,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: WalletInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, info);
    }
}
