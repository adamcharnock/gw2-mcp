//! Wallet types from the GW2 `/v2/account/wallet` endpoint.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::currency::{Currency, CurrencyId};

/// A single wallet line item: how much of one currency the account holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WalletEntry {
    pub id: CurrencyId,
    /// Quantity. Coin is in copper; other currencies use their natural unit.
    pub value: i64,
}

/// Aggregated wallet view returned to MCP clients.
///
/// Uses `BTreeMap` so JSON output is stable across runs — useful for
/// caching, snapshots, and tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WalletInfo {
    pub entries: Vec<WalletEntry>,
    pub currencies: BTreeMap<CurrencyId, Currency>,
    pub total_currencies: usize,
    pub updated_at: DateTime<Utc>,
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
    }

    #[test]
    fn wallet_info_round_trip() {
        let info = WalletInfo {
            entries: vec![WalletEntry {
                id: CurrencyId::new(1).unwrap(),
                value: 100,
            }],
            currencies: BTreeMap::new(),
            total_currencies: 1,
            updated_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: WalletInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, info);
    }
}
