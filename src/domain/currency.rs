//! Currency types from the GW2 `/v2/currencies` endpoint.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::error::DomainError;

/// A positive currency identifier.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct CurrencyId(u32);

impl CurrencyId {
    /// Construct a `CurrencyId` from any signed integer; rejects ≤ 0.
    pub fn new(id: i64) -> Result<Self, DomainError> {
        if id <= 0 || id > i64::from(u32::MAX) {
            return Err(DomainError::CurrencyIdInvalid { got: id });
        }
        // Safe: bounds checked above.
        Ok(Self(u32::try_from(id).expect("bounds checked")))
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for CurrencyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Metadata for a single currency.
///
/// Trimmed to the fields the LLM actually needs (`id`, `name`). Icon
/// URLs, description text, and the UI sort order are accepted on the
/// wire but discarded during deserialization — they were collectively
/// ~50% of the `get_wallet` response payload while contributing nothing
/// to LLM reasoning (the names are self-explanatory and the LLM can't
/// render images).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Currency {
    pub id: CurrencyId,
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn currency_id_rejects_zero_and_negative() {
        assert!(matches!(
            CurrencyId::new(0).unwrap_err(),
            DomainError::CurrencyIdInvalid { got: 0 }
        ));
        assert!(matches!(
            CurrencyId::new(-1).unwrap_err(),
            DomainError::CurrencyIdInvalid { got: -1 }
        ));
    }

    #[test]
    fn currency_id_accepts_positive() {
        let id = CurrencyId::new(42).unwrap();
        assert_eq!(id.get(), 42);
        assert_eq!(format!("{id}"), "42");
    }

    #[test]
    fn currency_id_serde_round_trip() {
        let id = CurrencyId::new(7).unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "7");
        let back: CurrencyId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn currency_deserialises_gw2_response_shape() {
        let raw = r#"{
            "id": 1,
            "name": "Coin",
            "description": "Gold, silver, and copper coins.",
            "icon": "https://render.guildwars2.com/file/coin.png",
            "order": 101
        }"#;
        let c: Currency = serde_json::from_str(raw).unwrap();
        assert_eq!(c.id, CurrencyId::new(1).unwrap());
        assert_eq!(c.name, "Coin");
    }
}
