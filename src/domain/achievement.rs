//! Achievement reference type from the GW2 API.
//!
//! Mirrors the [`Skill`](super::reference::Skill) shape: only `id` + `name`
//! are typed, everything else round-trips verbatim through `extra`. The GW2
//! `/v2/achievements` endpoint has many sub-shapes (tiers, rewards,
//! prerequisites, bits) and we don't need to model them statically — the LLM
//! is the consumer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::error::DomainError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AchievementId(u32);

impl AchievementId {
    pub fn new(id: i64) -> Result<Self, DomainError> {
        if id <= 0 || id > i64::from(u32::MAX) {
            return Err(DomainError::AchievementIdInvalid { got: id });
        }
        Ok(Self(u32::try_from(id).expect("bounds checked")))
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for AchievementId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Achievement {
    pub id: AchievementId,
    #[serde(default)]
    pub name: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_and_negative() {
        assert!(AchievementId::new(0).is_err());
        assert!(AchievementId::new(-1).is_err());
    }

    #[test]
    fn accepts_positive() {
        assert_eq!(AchievementId::new(1840).unwrap().get(), 1840);
    }

    #[test]
    fn round_trips_unknown_fields() {
        let raw = r#"{
            "id": 1840,
            "name": "Daily Completionist",
            "description": "Complete 3 dailies.",
            "requirement": "Complete 3 daily achievements.",
            "tiers": [{"count":1,"points":10}],
            "rewards": [{"type":"Item","id":70047,"count":1}],
            "flags": ["Daily"]
        }"#;
        let a: Achievement = serde_json::from_str(raw).unwrap();
        assert_eq!(a.id.get(), 1840);
        assert_eq!(a.name, "Daily Completionist");
        assert!(a.extra.contains_key("requirement"));
        let back = serde_json::to_value(&a).unwrap();
        assert_eq!(back["tiers"][0]["count"], 1);
        assert_eq!(back["flags"][0], "Daily");
    }
}
