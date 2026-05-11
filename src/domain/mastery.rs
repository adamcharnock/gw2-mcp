//! GW2 mastery track metadata, mirroring the shape of `/v2/masteries`.
//!
//! Each mastery (e.g. "Gliding", "Skyscale", "Skiff Pilot") has a name,
//! a region affiliation (Tyria / Maguuma / Desert / Tundra / Cantha /
//! `SkyWard` / Unknown), and an ordered list of levels with point + xp
//! costs. The account endpoint `/v2/account/masteries` returns just
//! `{id, level}`; this type is the "what does mastery 5 actually mean?"
//! lookup that pairs with it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::error::DomainError;

/// Strongly-typed mastery track id. The GW2 API surfaces these as
/// positive integers; we reject zero / negative at the boundary to keep
/// the type honest about what it can represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MasteryId(u32);

impl MasteryId {
    pub fn new(id: i64) -> Result<Self, DomainError> {
        if id <= 0 || id > i64::from(u32::MAX) {
            return Err(DomainError::MasteryIdInvalid { got: id });
        }
        Ok(Self(u32::try_from(id).expect("bounds checked")))
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for MasteryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mastery {
    pub id: MasteryId,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub requirement: String,
    /// `Tyria`, `Maguuma`, `Desert`, `Tundra`, `Cantha`, `SkyWard`, …
    /// Round-tripped verbatim from the API so we don't have to keep an
    /// enum in sync as `ArenaNet` ships new expansions.
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub order: u32,
    #[serde(default)]
    pub levels: Vec<MasteryLevel>,
    /// Any other fields the API surfaces (icon, background, …) — we
    /// round-trip them so the LLM gets the full picture without us
    /// having to model every new field by hand.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MasteryLevel {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub instruction: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub point_cost: u32,
    #[serde(default)]
    pub exp_cost: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_and_negative() {
        assert!(MasteryId::new(0).is_err());
        assert!(MasteryId::new(-1).is_err());
    }

    #[test]
    fn accepts_positive() {
        assert_eq!(MasteryId::new(1).unwrap().get(), 1);
        assert_eq!(MasteryId::new(42).unwrap().get(), 42);
    }

    #[test]
    fn round_trips_real_mastery_shape() {
        let raw = r#"{
            "id": 1,
            "name": "Exalted Acceptance",
            "requirement": "Earned by gaining the trust of the Exalted...",
            "order": 1,
            "background": "https://render.guildwars2.com/file/abcd.png",
            "region": "Maguuma",
            "levels": [
                {
                    "name": "Exalted Markings",
                    "description": "Translate.",
                    "instruction": "",
                    "icon": "https://render.guildwars2.com/file/icon.png",
                    "point_cost": 1,
                    "exp_cost": 254000
                }
            ]
        }"#;
        let m: Mastery = serde_json::from_str(raw).unwrap();
        assert_eq!(m.id.get(), 1);
        assert_eq!(m.name, "Exalted Acceptance");
        assert_eq!(m.region, "Maguuma");
        assert_eq!(m.levels.len(), 1);
        assert_eq!(m.levels[0].point_cost, 1);
        assert!(m.extra.contains_key("background"));
    }
}
